//! Refreshing the inline coordinates of surviving base ways under
//! `--locations-on-ways`.
//!
//! A way stores node IDs, and its version changes only when its tags or node
//! list change, so an OSC routinely moves a node without touching the ways
//! that use it. Under LocationsOnWays every such way carries the node's old
//! position inline. This module finds those ways and gives them the OSC's
//! coordinate.
//!
//! # The rule
//!
//! A surviving base way (one the OSC neither modifies nor deletes) needs a
//! refresh exactly when some ref names a node the OSC creates or modifies and
//! the way's inline coordinate at that position differs from the OSC's.
//! Each such position takes the OSC coordinate; every other position keeps
//! its own. Coordinates are compared and written in decimicrodegrees, the
//! unit every coordinate in this codebase is carried in: an inline value is
//! scaled under its block's framing (`offset + granularity * raw`
//! nanodegrees) and truncated to decimicrodegrees, as the base nodes of a
//! rewritten blob are. Under the default framing every producer writes, that
//! is exact.
//!
//! This is incremental maintenance, not validation: a coordinate that was
//! already stale for a node this OSC does not carry stays stale.
//!
//! - A ref to a node the OSC deletes keeps its coordinate and is counted.
//!   The OSM API refuses to delete a node a way still uses, so on a matched
//!   replication chain it does not happen; when it does, base and OSC
//!   disagree, and that alone never causes a rewrite.
//! - A tag-only node modify compares equal, so it causes no rewrite.
//! - A way with no coordinate arrays (both absent) is left as it is: there is
//!   no inline coordinate to compare or refresh. One array without the other,
//!   or arrays whose length differs from the refs', is malformed and refused,
//!   as `PrimitiveBlock` construction refuses it.
//!
//! # Who uses it
//!
//! The [`StaleProbe`] is built once from the OSC, before the pipeline
//! starts. It is deliberately separate from the coordinate map the drain
//! publishes at the node-to-way barrier: that map holds only nodes OSC ways
//! reference, and `loc_missing` is computed from its size, so it must not
//! grow with nodes only base ways use.
//!
//! Two callers share [`evaluate_way`], so they cannot disagree about which
//! way is stale:
//!
//! - detection ([`detect_stale_wire`], [`detect_stale_block`]), which decides
//!   whether a way blob the merge would otherwise pass through untouched has
//!   to be rewritten;
//! - the rewrite's surviving-base-way writer (`element_writes.rs`), which
//!   writes the refreshed coordinates through [`refreshed_locations`].

use protohoggr::{encode_varint, zigzag_decode_64, zigzag_encode_64};
use rustc_hash::FxHashMap;

use crate::error::{Result, new_wire_error};
use crate::osc::CompactDiffOverlay;
use crate::read::wire::{Cursor, WIRE_LEN, WIRE_VARINT, WireWay};

/// What the OSC says about one node a base way references.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ProbeEntry {
    /// Created or modified, with this `(lat, lon)` in decimicrodegrees.
    Upserted(i32, i32),
    /// Deleted by the OSC.
    Deleted,
}

/// Every node the OSC creates, modifies or deletes, with a small Bloom
/// prefilter in front of the map: on real daily diffs about one way ref in
/// ten thousand names such a node, so almost every probe is a miss.
///
/// The filter is blocked: both bits of a key live in one 64-bit word, so a
/// probe costs one memory access. At 16 bits per key its false-positive rate
/// is about 1.5%, and a false positive costs one map lookup.
pub(super) struct StaleProbe {
    words: Vec<u64>,
    /// `64 - log2(words.len())`: the hash's top bits pick the word.
    shift: u32,
    map: FxHashMap<i64, ProbeEntry>,
    /// Entries that are [`ProbeEntry::Upserted`].
    upserts: usize,
}

/// Bits of filter per probe key.
const FILTER_BITS_PER_KEY: usize = 16;

#[inline]
fn probe_hash(id: i64) -> u64 {
    id.cast_unsigned().wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

impl StaleProbe {
    /// Build from the parsed OSC. The overlay holds one final state per node
    /// (an upsert removes a delete and the reverse), so an ID is either
    /// upserted or deleted, never both.
    pub(super) fn from_diff(diff: &CompactDiffOverlay) -> Self {
        let deleted = diff.deleted_ids(crate::ElementKind::Node);
        let mut map: FxHashMap<i64, ProbeEntry> = FxHashMap::with_capacity_and_hasher(
            diff.node_count() + deleted.len(),
            Default::default(),
        );
        for &id in diff.node_ids() {
            if let Some(node) = diff.get_node(id) {
                map.insert(
                    id,
                    ProbeEntry::Upserted(node.decimicro_lat(), node.decimicro_lon()),
                );
            }
        }
        for &id in deleted {
            map.insert(id, ProbeEntry::Deleted);
        }
        Self::from_map(map)
    }

    /// A probe that matches nothing, for runs without `--locations-on-ways`.
    pub(super) fn empty() -> Self {
        Self::from_map(FxHashMap::default())
    }

    fn from_map(map: FxHashMap<i64, ProbeEntry>) -> Self {
        let bits = (map.len() * FILTER_BITS_PER_KEY)
            .next_power_of_two()
            .max(128);
        let words_len = bits / 64;
        let shift = 64 - words_len.trailing_zeros();
        let upserts = map
            .values()
            .filter(|e| matches!(e, ProbeEntry::Upserted(..)))
            .count();
        let mut probe = Self {
            words: vec![0; words_len],
            shift,
            map,
            upserts,
        };
        let keys: Vec<i64> = probe.map.keys().copied().collect();
        for id in keys {
            let (word, mask) = probe.slot(id);
            probe.words[word] |= mask;
        }
        probe
    }

    #[inline]
    fn slot(&self, id: i64) -> (usize, u64) {
        let h = probe_hash(id);
        #[allow(clippy::cast_possible_truncation)]
        let word = (h >> self.shift) as usize;
        let mask = (1u64 << ((h >> 20) & 63)) | (1u64 << ((h >> 26) & 63));
        (word, mask)
    }

    /// True when the OSC creates, modifies or deletes no node: no ref can hit
    /// the probe, so evaluating a way finds nothing.
    pub(super) fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// True when the OSC creates or modifies some node, so a way can be
    /// stale. Without one, only deleted-node refs could be found, which never
    /// cause a rewrite, so way blobs are not searched for them: the
    /// deleted-node diagnostic then covers only blobs rewritten anyway.
    pub(super) fn has_upserts(&self) -> bool {
        self.upserts > 0
    }

    /// Heap bytes held by the filter and the map (capacity, one control byte
    /// per bucket), an estimate like `CompactDiffOverlay::heap_size_estimate`.
    pub(super) fn heap_bytes(&self) -> usize {
        self.words.capacity() * std::mem::size_of::<u64>()
            + self.map.capacity() * (std::mem::size_of::<(i64, ProbeEntry)>() + 1)
    }

    /// Whether the Bloom filter alone admits `id` (test-only view of the
    /// prefilter the lookup consults first).
    #[cfg(test)]
    fn filter_admits(&self, id: i64) -> bool {
        let (word, mask) = self.slot(id);
        self.words[word] & mask == mask
    }

    /// What the OSC says about node `id`, if anything.
    #[inline]
    pub(super) fn get(&self, id: i64) -> Option<ProbeEntry> {
        let (word, mask) = self.slot(id);
        if self.words[word] & mask != mask {
            return None;
        }
        self.map.get(&id).copied()
    }
}

/// A block's coordinate framing: a coordinate is `offset + granularity *
/// raw` nanodegrees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CoordFraming {
    pub(super) granularity: i64,
    pub(super) lat_offset: i64,
    pub(super) lon_offset: i64,
}

impl CoordFraming {
    /// The PBF default, the only framing `BlockBuilder` writes.
    pub(super) const DEFAULT: Self = Self {
        granularity: 100,
        lat_offset: 0,
        lon_offset: 0,
    };

    pub(super) fn of_block(block: &crate::PrimitiveBlock) -> Self {
        let (granularity, lat_offset, lon_offset) = block.coord_framing();
        Self {
            granularity: i64::from(granularity),
            lat_offset,
            lon_offset,
        }
    }

    /// True when raw values already are decimicrodegrees, so raw bytes can be
    /// copied into a default-framed output block unchanged.
    pub(super) fn is_default(self) -> bool {
        self == Self::DEFAULT
    }

    /// `(lat, lon)` in decimicrodegrees from accumulated raw values, in
    /// checked arithmetic. Truncates toward zero, like the node scanners.
    #[inline]
    fn decimicro(self, raw_lat: i64, raw_lon: i64) -> Result<(i32, i32)> {
        Ok((
            scale(self.lat_offset, self.granularity, raw_lat)?,
            scale(self.lon_offset, self.granularity, raw_lon)?,
        ))
    }
}

#[inline]
fn scale(offset: i64, granularity: i64, raw: i64) -> Result<i32> {
    let nano = granularity
        .checked_mul(raw)
        .and_then(|scaled| offset.checked_add(scaled))
        .ok_or_else(|| {
            new_wire_error(
                "way node location arithmetic overflowed i64; the block's \
                 granularity, offset, or coordinate deltas are corrupt",
            )
        })?;
    i32::try_from(nano / 100).map_err(|_| {
        new_wire_error(
            "way node location does not fit in an i32 decimicrodegree; the \
             block's granularity, offset, or coordinate deltas are corrupt",
        )
    })
}

#[cold]
#[inline(never)]
fn refs_err() -> crate::error::Error {
    new_wire_error("way refs array holds a truncated or overlong varint")
}

#[cold]
#[inline(never)]
fn coords_err() -> crate::error::Error {
    new_wire_error(
        "way has a node-location array that is truncated, overlong, or of a \
         different length than its node references",
    )
}

/// Per-way outcome of [`evaluate_way`].
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct WayEval {
    /// Positions whose inline coordinate differs from the OSC's. Nonzero
    /// means the way must be refreshed.
    pub(super) stale_refs: u64,
    /// Positions naming a node the OSC deletes.
    pub(super) deleted_refs: u64,
    /// The way carries coordinate arrays.
    pub(super) has_coords: bool,
}

/// Evaluate one surviving base way from its raw packed `refs`, `lat` and
/// `lon` arrays under `framing`. `hits` is caller scratch.
///
/// The refs are always decoded, fallibly, and the coordinate arrays always
/// checked as `PrimitiveBlock` construction checks them: both absent, or
/// both made of complete varints and as long as the refs. They are decoded
/// only when some ref names a node the OSC upserts, and then every
/// coordinate is scaled checked, as [`refreshed_locations`] scales it.
pub(super) fn evaluate_way(
    refs_data: &[u8],
    lat_data: &[u8],
    lon_data: &[u8],
    framing: CoordFraming,
    probe: &StaleProbe,
    hits: &mut Vec<(usize, (i32, i32))>,
) -> Result<WayEval> {
    hits.clear();
    let mut eval = WayEval {
        has_coords: !(lat_data.is_empty() && lon_data.is_empty()),
        ..WayEval::default()
    };
    let mut refs = Cursor::new(refs_data);
    let mut current: i64 = 0;
    let mut index = 0usize;
    while !refs.is_empty() {
        let delta = zigzag_decode_64(refs.read_varint().map_err(|_| refs_err())?);
        current = current.wrapping_add(delta);
        match probe.get(current) {
            None => {}
            Some(ProbeEntry::Deleted) => eval.deleted_refs += 1,
            Some(ProbeEntry::Upserted(lat, lon)) => hits.push((index, (lat, lon))),
        }
        index += 1;
    }
    if !eval.has_coords {
        return Ok(eval);
    }
    if hits.is_empty() {
        let count = |data| crate::read::block::packed_varint_count(data).map_err(|_| coords_err());
        if count(lat_data)? != index || count(lon_data)? != index {
            return Err(coords_err());
        }
        return Ok(eval);
    }

    let mut lats = Cursor::new(lat_data);
    let mut lons = Cursor::new(lon_data);
    let mut raw_lat: i64 = 0;
    let mut raw_lon: i64 = 0;
    let mut next_hit = 0usize;
    for position in 0..index {
        let dlat = zigzag_decode_64(lats.read_varint().map_err(|_| coords_err())?);
        let dlon = zigzag_decode_64(lons.read_varint().map_err(|_| coords_err())?);
        raw_lat = raw_lat.wrapping_add(dlat);
        raw_lon = raw_lon.wrapping_add(dlon);
        let inline = framing.decimicro(raw_lat, raw_lon)?;
        if let Some(&(hit_position, osc)) = hits.get(next_hit)
            && hit_position == position
        {
            next_hit += 1;
            if inline != osc {
                eval.stale_refs += 1;
            }
        }
    }
    if !lats.is_empty() || !lons.is_empty() {
        return Err(coords_err());
    }
    Ok(eval)
}

/// The refreshed packed `lat` / `lon` arrays of one way, in the default
/// framing, into `out_lats` / `out_lons` (cleared first): every inline
/// coordinate scaled to decimicrodegrees, with each position naming a node
/// the OSC upserts taking the OSC's coordinate. Returns how many positions
/// changed value, which equals [`evaluate_way`]'s `stale_refs`.
///
/// The way must carry coordinate arrays; one without them is written as it
/// is by the caller.
pub(super) fn refreshed_locations(
    refs_data: &[u8],
    lat_data: &[u8],
    lon_data: &[u8],
    framing: CoordFraming,
    probe: &StaleProbe,
    out_lats: &mut Vec<u8>,
    out_lons: &mut Vec<u8>,
) -> Result<u64> {
    out_lats.clear();
    out_lons.clear();
    let mut refs = Cursor::new(refs_data);
    let mut lats = Cursor::new(lat_data);
    let mut lons = Cursor::new(lon_data);
    let (mut current, mut raw_lat, mut raw_lon) = (0i64, 0i64, 0i64);
    let (mut last_lat, mut last_lon) = (0i64, 0i64);
    let mut changed = 0u64;
    while !refs.is_empty() {
        current = current.wrapping_add(zigzag_decode_64(
            refs.read_varint().map_err(|_| refs_err())?,
        ));
        raw_lat = raw_lat.wrapping_add(zigzag_decode_64(
            lats.read_varint().map_err(|_| coords_err())?,
        ));
        raw_lon = raw_lon.wrapping_add(zigzag_decode_64(
            lons.read_varint().map_err(|_| coords_err())?,
        ));
        let inline = framing.decimicro(raw_lat, raw_lon)?;
        let (lat, lon) = match probe.get(current) {
            Some(ProbeEntry::Upserted(lat, lon)) => {
                if (lat, lon) != inline {
                    changed += 1;
                }
                (lat, lon)
            }
            _ => inline,
        };
        let (lat, lon) = (i64::from(lat), i64::from(lon));
        encode_varint(out_lats, zigzag_encode_64(lat - last_lat));
        encode_varint(out_lons, zigzag_encode_64(lon - last_lon));
        last_lat = lat;
        last_lon = lon;
    }
    if !lats.is_empty() || !lons.is_empty() {
        return Err(coords_err());
    }
    Ok(changed)
}

/// Outcome of detection over one way blob.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Detection {
    /// Some way needs a refresh. Detection stops at the first one, so the
    /// counts below are partial then; the caller discards them and the
    /// rewrite counts for itself.
    pub(super) stale: bool,
    /// Refs to OSC-deleted nodes. Complete when `stale` is false.
    pub(super) deleted_refs: u64,
}

/// Detection on a decompressed way blob that is not parsed into a
/// `PrimitiveBlock`: the blob would be passed through untouched unless a way
/// in it is stale, and a full parse of every such blob would cost more than
/// the refs walk detection needs.
///
/// Only what detection reads is validated: the block and group framing, each
/// Way message, its refs and the shape of its coordinate arrays (as
/// [`evaluate_way`] checks them), and the coordinate values of ways with a
/// probe hit. Tags, Info and the string table are not read. A blob found clean is copied as it is, exactly as before this detection
/// existed, when it was not even decompressed; a stale one is parsed in full
/// before it is rewritten. The framing fields may follow the groups and the
/// last occurrence wins, as in the block parser, so groups are collected
/// before any way is evaluated.
pub(super) fn detect_stale_wire(
    decompressed: &[u8],
    probe: &StaleProbe,
    groups: &mut Vec<(usize, usize)>,
    hits: &mut Vec<(usize, (i32, i32))>,
) -> Result<Detection> {
    groups.clear();
    let mut framing = CoordFraming::DEFAULT;
    let mut cursor = Cursor::new(decompressed);
    while let Some((field, wire_type)) = cursor.read_tag()? {
        match (field, wire_type) {
            (2, WIRE_LEN) => {
                let data = cursor.read_len_delimited()?;
                let offset = data.as_ptr() as usize - decompressed.as_ptr() as usize;
                groups.push((offset, data.len()));
            }
            (17, WIRE_VARINT) => {
                framing.granularity = i64::from(crate::read::wire::read_int32(
                    &mut cursor,
                    "PrimitiveBlock.granularity exceeds int32",
                )?);
            }
            (19, WIRE_VARINT) => framing.lat_offset = cursor.read_varint_i64()?,
            (20, WIRE_VARINT) => framing.lon_offset = cursor.read_varint_i64()?,
            _ => cursor.skip_field(wire_type)?,
        }
    }

    let mut detection = Detection::default();
    for &(offset, len) in groups.iter() {
        let mut group = Cursor::new(&decompressed[offset..offset + len]);
        while let Some((field, wire_type)) = group.read_tag()? {
            if field == 3 && wire_type == WIRE_LEN {
                let way = WireWay::parse(group.read_len_delimited()?)?;
                let eval = evaluate_way(
                    way.refs_data,
                    way.lat_data,
                    way.lon_data,
                    framing,
                    probe,
                    hits,
                )?;
                if eval.stale_refs > 0 {
                    detection.stale = true;
                    return Ok(detection);
                }
                detection.deleted_refs += eval.deleted_refs;
            } else {
                group.skip_field(wire_type)?;
            }
        }
    }
    Ok(detection)
}

/// Detection on a way blob already parsed (and so validated) into a
/// `PrimitiveBlock`, whose ways all survive the merge.
pub(super) fn detect_stale_block(
    block: &crate::PrimitiveBlock,
    probe: &StaleProbe,
    hits: &mut Vec<(usize, (i32, i32))>,
) -> Result<Detection> {
    let framing = CoordFraming::of_block(block);
    let mut detection = Detection::default();
    for element in block.elements_skip_metadata() {
        if let crate::Element::Way(way) = element {
            let eval = evaluate_way(
                way.refs_data(),
                way.lat_data(),
                way.lon_data(),
                framing,
                probe,
                hits,
            )?;
            if eval.stale_refs > 0 {
                detection.stale = true;
                return Ok(detection);
            }
            detection.deleted_refs += eval.deleted_refs;
        }
    }
    Ok(detection)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn packed(values: &[i64]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut last = 0i64;
        for &v in values {
            encode_varint(&mut out, zigzag_encode_64(v.wrapping_sub(last)));
            last = v;
        }
        out
    }

    fn probe(entries: &[(i64, ProbeEntry)]) -> StaleProbe {
        StaleProbe::from_map(entries.iter().copied().collect())
    }

    #[test]
    fn probe_filter_admits_every_key_and_rejects_most_absent_ones() {
        let entries: Vec<(i64, ProbeEntry)> = (0..5000)
            .map(|i| (i * 7919, ProbeEntry::Upserted(1, 2)))
            .collect();
        let p = probe(&entries);
        for &(id, entry) in &entries {
            assert!(p.filter_admits(id), "the filter must never reject a key");
            assert_eq!(p.get(id), Some(entry));
        }
        // The filter itself, not the exact lookup behind it, turns away
        // nearly every absent ID: about 1.5% at 16 bits per key.
        let absent = (1..20_000).map(|i| i * 7919 + 1);
        let admitted = absent.clone().filter(|&id| p.filter_admits(id)).count();
        assert!(
            admitted < 20_000 / 20,
            "{admitted} of 20000 absent IDs admitted"
        );
        assert!(absent.into_iter().all(|id| p.get(id).is_none()));

        assert!(probe(&[]).is_empty());
        assert_eq!(probe(&[]).get(5), None);
        let deletes = probe(&[(5, ProbeEntry::Deleted)]);
        assert!(!deletes.is_empty() && !deletes.has_upserts());
        assert!(p.has_upserts());
    }

    fn varint(v: u64, out: &mut Vec<u8>) {
        encode_varint(out, v);
    }

    fn field_len(field: u32, payload: &[u8], out: &mut Vec<u8>) {
        varint(u64::from((field << 3) | 2), out);
        varint(payload.len() as u64, out);
        out.extend_from_slice(payload);
    }

    fn field_varint(field: u32, value: i64, out: &mut Vec<u8>) {
        varint(u64::from(field << 3), out);
        varint(value.cast_unsigned(), out);
    }

    /// A Way message with raw coordinate values.
    fn way(id: i64, refs: &[i64], lats: &[i64], lons: &[i64]) -> Vec<u8> {
        let mut out = Vec::new();
        field_varint(1, id, &mut out);
        field_len(8, &packed(refs), &mut out);
        field_len(9, &packed(lats), &mut out);
        field_len(10, &packed(lons), &mut out);
        out
    }

    fn group(ways: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        for w in ways {
            field_len(3, w, &mut out);
        }
        out
    }

    /// A block: string table, the groups, then the framing fields in the
    /// order given (after the groups, as the format allows).
    fn block(groups: &[Vec<u8>], framing: &[(u32, i64)]) -> Vec<u8> {
        let mut out = Vec::new();
        field_len(1, &[0x0A, 0x00], &mut out);
        for g in groups {
            field_len(2, g, &mut out);
        }
        for &(field, value) in framing {
            field_varint(field, value, &mut out);
        }
        out
    }

    fn detect(block: &[u8], p: &StaleProbe) -> Detection {
        detect_stale_wire(block, p, &mut Vec::new(), &mut Vec::new()).unwrap()
    }

    /// Way 2, in the second group, refs node 7 at raw (3, 1). Under
    /// granularity 1000 and lat_offset 500 that is (35, 10) decimicro; under
    /// the default it would be (3, 1).
    fn two_group_block(framing: &[(u32, i64)]) -> Vec<u8> {
        block(
            &[
                group(&[way(1, &[6], &[9], &[9])]),
                group(&[way(2, &[5, 7], &[0, 3], &[0, 1])]),
            ],
            framing,
        )
    }

    const SCALED: [(u32, i64); 2] = [(17, 1000), (19, 500)];

    /// The wire detector reads the framing fields wherever they sit, after
    /// the groups included, and scales before comparing: an OSC coordinate
    /// equal to the scaled value is clean, one equal only to the raw value is
    /// stale. Ignoring the framing would invert both answers.
    #[test]
    fn wire_detection_applies_framing_that_follows_the_groups() {
        let block = two_group_block(&SCALED);
        let scaled = probe(&[(7, ProbeEntry::Upserted(35, 10))]);
        let raw = probe(&[(7, ProbeEntry::Upserted(3, 1))]);
        assert!(!detect(&block, &scaled).stale);
        assert!(detect(&block, &raw).stale);
        // And the default framing reads raw values as decimicro.
        let plain = two_group_block(&[]);
        assert!(!detect(&plain, &raw).stale);
        assert!(detect(&plain, &scaled).stale);
    }

    /// A framing field given twice takes its last value, as in the block
    /// parser.
    #[test]
    fn wire_detection_takes_the_last_of_repeated_framing_fields() {
        let scaled = probe(&[(7, ProbeEntry::Upserted(35, 10))]);
        let last_scaled = two_group_block(&[(17, 100), (19, 0), (17, 1000), (19, 500)]);
        assert!(!detect(&last_scaled, &scaled).stale);
        let last_default = two_group_block(&[(17, 1000), (19, 500), (17, 100), (19, 0)]);
        assert!(detect(&last_default, &scaled).stale);
    }

    /// A clean blob's deleted-node refs are counted across every group.
    #[test]
    fn wire_detection_counts_deleted_refs_in_every_group() {
        let p = probe(&[(6, ProbeEntry::Deleted), (5, ProbeEntry::Deleted)]);
        let found = detect(&two_group_block(&SCALED), &p);
        assert!(!found.stale);
        assert_eq!(found.deleted_refs, 2);
    }

    #[test]
    fn a_differing_upserted_ref_is_stale_and_an_equal_one_is_not() {
        let refs = packed(&[1, 2, 3]);
        let lats = packed(&[10, 20, 30]);
        let lons = packed(&[11, 21, 31]);
        let mut hits = Vec::new();
        let p = probe(&[(2, ProbeEntry::Upserted(20, 21))]);
        let eval = evaluate_way(&refs, &lats, &lons, CoordFraming::DEFAULT, &p, &mut hits).unwrap();
        assert_eq!(eval.stale_refs, 0, "a tag-only modify compares equal");
        let p = probe(&[(2, ProbeEntry::Upserted(20, 99))]);
        let eval = evaluate_way(&refs, &lats, &lons, CoordFraming::DEFAULT, &p, &mut hits).unwrap();
        assert_eq!(eval.stale_refs, 1, "a longitude-only move is one stale ref");
    }

    #[test]
    fn repeated_refs_count_and_refresh_every_occurrence() {
        let refs = packed(&[5, 6, 5]);
        let lats = packed(&[10, 20, 10]);
        let lons = packed(&[11, 21, 11]);
        let p = probe(&[(5, ProbeEntry::Upserted(-7, 8)), (6, ProbeEntry::Deleted)]);
        let eval = evaluate_way(
            &refs,
            &lats,
            &lons,
            CoordFraming::DEFAULT,
            &p,
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!((eval.stale_refs, eval.deleted_refs), (2, 1));
        let (mut out_lats, mut out_lons) = (Vec::new(), Vec::new());
        let changed = refreshed_locations(
            &refs,
            &lats,
            &lons,
            CoordFraming::DEFAULT,
            &p,
            &mut out_lats,
            &mut out_lons,
        )
        .unwrap();
        assert_eq!(changed, 2);
        assert_eq!(out_lats, packed(&[-7, 20, -7]));
        assert_eq!(out_lons, packed(&[8, 21, 8]));
    }

    #[test]
    fn coordinates_compare_under_the_block_framing() {
        // granularity 1000 nanodegrees, lat offset 500 nanodegrees: raw 3
        // is 3500 nanodegrees = 35 decimicrodegrees.
        let framing = CoordFraming {
            granularity: 1000,
            lat_offset: 500,
            lon_offset: 0,
        };
        let refs = packed(&[1, 2]);
        let lats = packed(&[3, 4]);
        let lons = packed(&[1, 2]);
        let p = probe(&[(1, ProbeEntry::Upserted(35, 10))]);
        let eval = evaluate_way(&refs, &lats, &lons, framing, &p, &mut Vec::new()).unwrap();
        assert_eq!(
            eval.stale_refs, 0,
            "equal once scaled, though the raw bytes differ"
        );
        let (mut out_lats, mut out_lons) = (Vec::new(), Vec::new());
        let changed = refreshed_locations(
            &refs,
            &lats,
            &lons,
            framing,
            &p,
            &mut out_lats,
            &mut out_lons,
        )
        .unwrap();
        assert_eq!(changed, 0);
        assert_eq!(
            out_lats,
            packed(&[35, 45]),
            "normalized to the default framing"
        );
        assert_eq!(out_lons, packed(&[10, 20]));
    }

    #[test]
    fn coordinate_free_ways_are_exempt_and_partial_arrays_are_refused() {
        let refs = packed(&[1, 2]);
        let p = probe(&[(1, ProbeEntry::Upserted(1, 1)), (2, ProbeEntry::Deleted)]);
        let eval =
            evaluate_way(&refs, &[], &[], CoordFraming::DEFAULT, &p, &mut Vec::new()).unwrap();
        assert_eq!(
            (eval.stale_refs, eval.deleted_refs, eval.has_coords),
            (0, 1, false)
        );
        let lats = packed(&[5, 6]);
        assert!(
            evaluate_way(
                &refs,
                &lats,
                &[],
                CoordFraming::DEFAULT,
                &p,
                &mut Vec::new()
            )
            .is_err()
        );
        let short = packed(&[5]);
        assert!(
            evaluate_way(
                &refs,
                &short,
                &short,
                CoordFraming::DEFAULT,
                &p,
                &mut Vec::new()
            )
            .is_err()
        );
        let long = packed(&[5, 6, 7]);
        assert!(
            evaluate_way(
                &refs,
                &long,
                &long,
                CoordFraming::DEFAULT,
                &p,
                &mut Vec::new()
            )
            .is_err()
        );
    }

    /// Array shapes are checked whether or not a ref hits the probe, and a
    /// hit way's every coordinate is scaled checked, not only the hit's.
    #[test]
    fn malformed_coordinates_are_refused_without_an_upsert_hit() {
        let refs = packed(&[1, 2]);
        let p = probe(&[(1, ProbeEntry::Deleted), (9, ProbeEntry::Upserted(0, 0))]);
        let lats = packed(&[5, 6]);
        let short = packed(&[5]);
        let d = CoordFraming::DEFAULT;
        assert!(evaluate_way(&refs, &lats, &[], d, &p, &mut Vec::new()).is_err());
        assert!(evaluate_way(&refs, &lats, &short, d, &p, &mut Vec::new()).is_err());
        assert!(evaluate_way(&refs, &lats, &[0x80], d, &p, &mut Vec::new()).is_err());
        assert!(evaluate_way(&refs, &lats, &lats, d, &p, &mut Vec::new()).is_ok());

        let p = probe(&[(1, ProbeEntry::Upserted(5, 5))]);
        let wild = packed(&[5, i64::from(i32::MAX) * 1000]);
        assert!(evaluate_way(&refs, &wild, &lats, d, &p, &mut Vec::new()).is_err());
    }

    #[test]
    fn truncated_refs_are_refused() {
        let p = probe(&[(1, ProbeEntry::Deleted)]);
        assert!(
            evaluate_way(
                &[0x02, 0x80],
                &[],
                &[],
                CoordFraming::DEFAULT,
                &p,
                &mut Vec::new()
            )
            .is_err()
        );
    }
}
