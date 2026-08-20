//! Way-ref-only wire-format scanner for extracting way node references from PBF blobs.
//!
//! Bypasses [`PrimitiveBlock`] construction - no string table parsing,
//! no group_ranges allocation. Only extracts way IDs and their node ref lists.
//!
//! Used by passes that only need `way.id()` + `way.refs()`:
//! - ALTW pass 0 (`collect_way_referenced_node_ids`)
//! - Geocode builder pass 1.5 (referenced node collection)
//!
//! # Known limitations
//!
//! - **Way groups only.** Parses PrimitiveGroup field 3 (Way). Other element
//!   types (nodes, relations) in the same group are skipped.
//! - **Sorted PBF assumption.** Relies on indexdata `ElemKind::Way` for blob
//!   filtering. Mixed-type blobs in unsorted PBFs could be mislabeled.

use crate::error::Result;

/// Classification flags emitted by [`scan_way_geocode_tagged_refs`] per
/// matching way. The caller combines these with its own
/// `needed_admin_ways` membership test to decide whether to consume
/// the refs.
pub(crate) struct WayGeocodeFlags {
    pub(crate) is_street: bool,
    pub(crate) is_building_addr: bool,
    pub(crate) is_interp: bool,
}

/// Geocode-relevant tag keys + the values that exclude a way from the
/// street set. Pre-encode once per caller and reuse across blobs.
pub(crate) struct GeocodeTagLiterals<'a> {
    pub(crate) k_highway: &'a [u8],
    pub(crate) k_name: &'a [u8],
    pub(crate) k_addr_housenumber: &'a [u8],
    pub(crate) k_addr_street: &'a [u8],
    pub(crate) k_building: &'a [u8],
    pub(crate) k_addr_interpolation: &'a [u8],
    pub(crate) excluded_highway_values: &'a [&'a [u8]],
}

impl GeocodeTagLiterals<'_> {
    pub(crate) const fn standard() -> GeocodeTagLiterals<'static> {
        GeocodeTagLiterals {
            k_highway: b"highway",
            k_name: b"name",
            k_addr_housenumber: b"addr:housenumber",
            k_addr_street: b"addr:street",
            k_building: b"building",
            k_addr_interpolation: b"addr:interpolation",
            excluded_highway_values: &[
                b"footway",
                b"path",
                b"track",
                b"steps",
                b"cycleway",
                b"service",
                b"pedestrian",
                b"bridleway",
                b"construction",
            ],
        }
    }
}

/// Per-blob resolved string-table indices for the geocode tag literals.
/// `None` means the literal is absent from this blob's string table -
/// e.g. a blob with no building-addr ways may legitimately lack the
/// `"building"` key string. Resolved once per blob before the way loop.
#[derive(Default)]
struct ResolvedTagIndices {
    k_highway: Option<u32>,
    k_name: Option<u32>,
    k_addr_housenumber: Option<u32>,
    k_addr_street: Option<u32>,
    k_building: Option<u32>,
    k_addr_interpolation: Option<u32>,
    // Bitmask over `excluded_highway_values` indices: if the highway
    // value's string-table index matches any entry we set here, the way
    // is excluded from streets. Small fixed size to avoid allocating
    // per-blob; ExcludedHighwaysMask has 9 slots matching
    // `GeocodeTagLiterals::excluded_highway_values`.
    excluded_highway_indices: [Option<u32>; 9],
}

/// Extract way IDs and their node refs from decompressed PrimitiveBlock bytes.
///
/// For each way, calls `callback(way_id, &refs)` where refs is the decoded
/// node ID list. Uses a caller-provided `refs_buf` to avoid per-way allocation.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
pub(crate) fn scan_way_refs(
    decompressed: &[u8],
    refs_buf: &mut Vec<i64>,
    group_starts: &mut Vec<(usize, usize)>,
    mut callback: impl FnMut(i64, &[i64]),
) -> Result<()> {
    use crate::read::wire::{Cursor, WIRE_LEN};

    let buffer = decompressed;
    let mut cursor = Cursor::new(buffer);
    group_starts.clear();

    // Parse PrimitiveBlock top-level: only collect group offsets.
    while let Some((field, wire_type)) = cursor.read_tag()? {
        match (field, wire_type) {
            (2, WIRE_LEN) => {
                let data = cursor.read_len_delimited()?;
                let offset = data.as_ptr() as usize - buffer.as_ptr() as usize;
                group_starts.push((offset, data.len()));
            }
            _ => {
                cursor.skip_field(wire_type)?;
            }
        }
    }

    for &(off, len) in group_starts.iter() {
        let group_data = &buffer[off..off + len];
        let mut gcursor = Cursor::new(group_data);

        // PrimitiveGroup field 3 = Way (repeated).
        while let Some((field, wire_type)) = gcursor.read_tag()? {
            if field == 3 && wire_type == WIRE_LEN {
                let way_data = gcursor.read_len_delimited()?;
                parse_way_refs(way_data, refs_buf, &mut callback)?;
            } else {
                gcursor.skip_field(wire_type)?;
            }
        }
    }

    Ok(())
}

/// Parse a single Way message and extract id + refs.
///
/// Routes through [`WireWay::parse`] rather than hand-rolling a third Way
/// parser. That matters for more than duplication: `wire.rs` accepts a packed
/// repeated field in *either* legal protobuf wire form (a length-delimited
/// packed segment, or individually tagged varints), and this scanner used to
/// match `WIRE_LEN` only. On a spec-legal unpacked encoding the old code saw
/// `refs` as absent, invoked no callback at all, and the caller concluded the
/// way had no node references - a silently truncated node index, not an error.
/// See `reference/truncation-handling.md`, "Which wire forms of a packed field
/// count as present".
fn parse_way_refs(
    way_data: &[u8],
    refs_buf: &mut Vec<i64>,
    callback: &mut impl FnMut(i64, &[i64]),
) -> Result<()> {
    use crate::read::wire::{PackedSint64Iter, WireWay};

    let way = WireWay::parse(way_data)?;
    if way.refs_data.is_empty() {
        return Ok(());
    }

    refs_buf.clear();
    let mut cum: i64 = 0;
    for delta in PackedSint64Iter::new(way.refs_data) {
        cum += delta;
        refs_buf.push(cum);
    }
    callback(way.id, refs_buf);

    Ok(())
}

/// Scan a decompressed PBF way blob and invoke `callback` for each way
/// that matches any of the geocode tag predicates (street /
/// building-addr / interp). Bypasses full [`PrimitiveBlock`]
/// construction: resolves geocode tag literals once per blob against
/// the raw string table, then walks each Way message parsing only id,
/// keys, vals, and refs.
///
/// The callback receives `(way_id, flags, refs)`. The caller applies
/// any additional admin-way-membership test (by `way_id`) and decides
/// whether to consume the refs.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cognitive_complexity,
    clippy::too_many_lines
)]
pub(crate) fn scan_way_geocode_tagged_refs(
    decompressed: &[u8],
    literals: &GeocodeTagLiterals<'_>,
    refs_buf: &mut Vec<i64>,
    group_starts: &mut Vec<(usize, usize)>,
    mut callback: impl FnMut(i64, WayGeocodeFlags, &[i64]),
) -> Result<()> {
    use crate::read::wire::{Cursor, WIRE_LEN};

    let buffer = decompressed;
    let mut cursor = Cursor::new(buffer);
    group_starts.clear();

    // Parse PrimitiveBlock top-level: string table (field 1) + groups (field 2).
    let mut stringtable_data: Option<&[u8]> = None;
    while let Some((field, wire_type)) = cursor.read_tag()? {
        match (field, wire_type) {
            (1, WIRE_LEN) => {
                stringtable_data = Some(cursor.read_len_delimited()?);
            }
            (2, WIRE_LEN) => {
                let data = cursor.read_len_delimited()?;
                let offset = data.as_ptr() as usize - buffer.as_ptr() as usize;
                group_starts.push((offset, data.len()));
            }
            _ => {
                cursor.skip_field(wire_type)?;
            }
        }
    }

    // Resolve string-table indices of geocode literals. We iterate the
    // StringTable's repeated `s` (field 1) entries once; every literal
    // we match stops being looked up for the rest of the blob.
    let resolved = if let Some(st_data) = stringtable_data {
        resolve_tag_indices(st_data, literals)?
    } else {
        ResolvedTagIndices::default()
    };

    // Fast-exit: if the blob's string table lacks all geocode-relevant
    // keys, no way in it can be geocode-relevant (callers can still
    // filter by admin membership via way_id outside this function, but
    // we stream every way's id+refs for them to decide). When none of
    // our keys are present, skip tag parsing entirely - just emit
    // (way_id, all-false flags, refs).
    let any_key_present = resolved.k_highway.is_some()
        || resolved.k_name.is_some()
        || resolved.k_addr_housenumber.is_some()
        || resolved.k_addr_street.is_some()
        || resolved.k_building.is_some()
        || resolved.k_addr_interpolation.is_some();

    for &(off, len) in group_starts.iter() {
        let group_data = &buffer[off..off + len];
        let mut gcursor = Cursor::new(group_data);

        // PrimitiveGroup field 3 = Way (repeated).
        while let Some((field, wire_type)) = gcursor.read_tag()? {
            if field == 3 && wire_type == WIRE_LEN {
                let way_data = gcursor.read_len_delimited()?;
                parse_way_tagged_refs(
                    way_data,
                    &resolved,
                    any_key_present,
                    refs_buf,
                    &mut callback,
                )?;
            } else {
                gcursor.skip_field(wire_type)?;
            }
        }
    }

    Ok(())
}

/// Scan a StringTable protobuf payload to find the indices of the
/// geocode tag literals (keys + excluded highway values). Returns
/// `Option<u32>` per literal - `None` means the string wasn't in the
/// table. Index 0 is reserved as the "delta coding baseline" by the
/// PBF spec; returned as 0 if a literal actually matches the first
/// entry (unlikely in practice but handled correctly).
fn resolve_tag_indices(
    stringtable_data: &[u8],
    literals: &GeocodeTagLiterals<'_>,
) -> Result<ResolvedTagIndices> {
    use crate::read::wire::{Cursor, WIRE_LEN};

    let mut resolved = ResolvedTagIndices::default();
    let mut cursor = Cursor::new(stringtable_data);
    let mut idx: u32 = 0;
    while let Some((field, wire_type)) = cursor.read_tag()? {
        if field == 1 && wire_type == WIRE_LEN {
            let s = cursor.read_len_delimited()?;
            // Match against each literal. Stop-on-first-match isn't
            // worth the branch - `eq` on short byte slices is cheap.
            if s == literals.k_highway && resolved.k_highway.is_none() {
                resolved.k_highway = Some(idx);
            }
            if s == literals.k_name && resolved.k_name.is_none() {
                resolved.k_name = Some(idx);
            }
            if s == literals.k_addr_housenumber && resolved.k_addr_housenumber.is_none() {
                resolved.k_addr_housenumber = Some(idx);
            }
            if s == literals.k_addr_street && resolved.k_addr_street.is_none() {
                resolved.k_addr_street = Some(idx);
            }
            if s == literals.k_building && resolved.k_building.is_none() {
                resolved.k_building = Some(idx);
            }
            if s == literals.k_addr_interpolation && resolved.k_addr_interpolation.is_none() {
                resolved.k_addr_interpolation = Some(idx);
            }
            for (i, &excl) in literals.excluded_highway_values.iter().enumerate() {
                if i < resolved.excluded_highway_indices.len()
                    && s == excl
                    && resolved.excluded_highway_indices[i].is_none()
                {
                    resolved.excluded_highway_indices[i] = Some(idx);
                }
            }
            idx += 1;
        } else {
            cursor.skip_field(wire_type)?;
        }
    }
    Ok(resolved)
}

/// Parse a single Way message: extract id, iterate keys/vals looking
/// for the geocode tag keys, extract refs if the way matches any
/// predicate (or if the caller may want refs for admin-way dispatch -
/// we stream all ways when no geocode keys are present in the blob's
/// string table).
#[allow(clippy::cast_possible_wrap)]
fn parse_way_tagged_refs(
    way_data: &[u8],
    resolved: &ResolvedTagIndices,
    any_key_present: bool,
    refs_buf: &mut Vec<i64>,
    callback: &mut impl FnMut(i64, WayGeocodeFlags, &[i64]),
) -> Result<()> {
    use crate::read::wire::{PackedSint64Iter, PackedUint32Iter, WireWay};

    // Same routing rationale as `parse_way_refs`: `WireWay::parse` accepts
    // both legal wire forms of a packed field, where this scanner's own
    // matcher accepted only the length-delimited one and read an unpacked
    // `keys` / `vals` / `refs` as empty.
    let way = WireWay::parse(way_data)?;
    let way_id = way.id;
    let keys_data = (!way.keys_data.is_empty()).then_some(way.keys_data);
    let vals_data = (!way.vals_data.is_empty()).then_some(way.vals_data);
    let refs_data = (!way.refs_data.is_empty()).then_some(way.refs_data);

    // Classify by iterating keys/vals in lockstep. Flags track key
    // presence + the specific highway value for the excluded-set check.
    let mut has_highway = false;
    let mut highway_val_idx: Option<u32> = None;
    let mut has_name = false;
    let mut has_addr_hn = false;
    let mut has_addr_st = false;
    let mut has_building = false;
    let mut has_addr_interp = false;

    if any_key_present && let (Some(kd), Some(vd)) = (keys_data, vals_data) {
        let keys = PackedUint32Iter::new(kd);
        let mut vals = PackedUint32Iter::new(vd);
        for key_idx in keys {
            let val_idx = vals.next();
            if Some(key_idx) == resolved.k_highway {
                has_highway = true;
                highway_val_idx = val_idx;
            } else if Some(key_idx) == resolved.k_name {
                has_name = true;
            } else if Some(key_idx) == resolved.k_addr_housenumber {
                has_addr_hn = true;
            } else if Some(key_idx) == resolved.k_addr_street {
                has_addr_st = true;
            } else if Some(key_idx) == resolved.k_building {
                has_building = true;
            } else if Some(key_idx) == resolved.k_addr_interpolation {
                has_addr_interp = true;
            }
        }
    }

    let is_street = has_highway && has_name && {
        // Not in any excluded highway value's index set.
        !resolved.excluded_highway_indices.iter().any(|&maybe_idx| {
            match (maybe_idx, highway_val_idx) {
                (Some(excl), Some(val)) => excl == val,
                _ => false,
            }
        })
    };
    let is_building_addr = has_building && has_addr_hn && has_addr_st;
    let is_interp = has_addr_interp && has_addr_st;

    // Always decode refs - the caller's admin-way membership test sits
    // outside this function and needs every way's refs when the id
    // matches. Cheap because refs are already size-capped by the OSM
    // 2000-ref convention.
    if let Some(rd) = refs_data {
        refs_buf.clear();
        let mut cum: i64 = 0;
        for delta in PackedSint64Iter::new(rd) {
            cum += delta;
            refs_buf.push(cum);
        }
    } else {
        refs_buf.clear();
    }

    callback(
        way_id,
        WayGeocodeFlags {
            is_street,
            is_building_addr,
            is_interp,
        },
        refs_buf,
    );

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// PrimitiveBlock holding one PrimitiveGroup holding one Way. `refs` is
    /// written packed when `packed`, unpacked (one varint-typed field 8 per
    /// delta) otherwise. Both spellings are legal protobuf.
    fn way_block(way_id: i64, refs: &[i64], packed: bool) -> Vec<u8> {
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
        #[allow(clippy::cast_sign_loss)]
        fn zigzag(v: i64) -> u64 {
            ((v << 1) ^ (v >> 63)) as u64
        }

        let mut way = Vec::new();
        way.push(0x08); // field 1 (id), varint
        #[allow(clippy::cast_sign_loss)]
        varint(way_id as u64, &mut way);

        let mut deltas = Vec::new();
        let mut prev = 0_i64;
        for &r in refs {
            deltas.push(zigzag(r - prev));
            prev = r;
        }
        if packed {
            let mut seg = Vec::new();
            for &d in &deltas {
                varint(d, &mut seg);
            }
            way.push(0x42); // field 8, LEN
            varint(seg.len() as u64, &mut way);
            way.extend_from_slice(&seg);
        } else {
            for &d in &deltas {
                way.push(0x40); // field 8, VARINT
                varint(d, &mut way);
            }
        }

        let mut group = Vec::new();
        group.push(0x1A); // field 3 (ways), LEN
        varint(way.len() as u64, &mut group);
        group.extend_from_slice(&way);

        let mut block = Vec::new();
        block.push(0x12); // PrimitiveBlock field 2, LEN
        varint(group.len() as u64, &mut block);
        block.extend_from_slice(&group);
        block
    }

    fn collect(block: &[u8]) -> Vec<(i64, Vec<i64>)> {
        let mut out = Vec::new();
        let mut refs_buf = Vec::new();
        let mut group_starts = Vec::new();
        scan_way_refs(block, &mut refs_buf, &mut group_starts, |id, refs| {
            out.push((id, refs.to_vec()));
        })
        .unwrap();
        out
    }

    #[test]
    fn a_single_unpacked_ref_is_not_read_as_no_refs() {
        // The old matcher took `(8, WIRE_LEN)` only, so an unpacked `refs`
        // fell to `skip_field`, `refs_data` stayed `None`, and the callback
        // never fired: the caller concluded the way referenced no nodes and
        // the node index came out silently short. A single unpacked varint is
        // byte-identical to a one-element packed segment.
        let unpacked = collect(&way_block(42, &[7], false));
        assert_eq!(
            unpacked,
            vec![(42, vec![7])],
            "an unpacked Way.refs must decode to the same refs a packed one does"
        );

        // Passing counterpart in the same shape: the packed spelling of the
        // identical way must give the identical answer, so this is not
        // satisfied by a scanner that emits refs unconditionally.
        assert_eq!(collect(&way_block(42, &[7], true)), unpacked);
    }

    #[test]
    fn a_way_with_no_refs_yields_no_callback() {
        assert!(
            collect(&way_block(42, &[], true)).is_empty(),
            "a way with an empty refs array contributes nothing"
        );
    }
}
