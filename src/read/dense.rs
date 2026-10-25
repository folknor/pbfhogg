//! Iterate over the dense nodes in a `PrimitiveGroup`

use super::block::{get_stringtable_key_value, str_from_stringtable, validated};
use super::wire::{
    Cursor, PackedBoolIter, PackedInt32Iter, PackedSint32Iter, PackedSint64Iter, WireBlock,
    WireDenseInfo, WireDenseNodes,
};
use crate::error::Result;

/// An OpenStreetMap node element from a compressed array of dense nodes.
#[derive(Clone, Debug)]
pub struct DenseNode<'a> {
    block: &'a WireBlock<'static>,

    /// The node id.
    pub(crate) id: i64,
    lat: i64,
    lon: i64,
    /// Raw packed varint bytes for this node's tags (key/value pairs, no delimiter).
    tag_bytes: &'a [u8],
    info: Option<DenseNodeInfo<'a>>,
    granularity: i64,
    lat_offset: i64,
    lon_offset: i64,
}

impl<'a> DenseNode<'a> {
    /// Returns the node id.
    #[inline]
    pub fn id(&self) -> i64 {
        self.id
    }

    /// return optional metadata about the node
    #[inline]
    pub fn info(&'a self) -> Option<&'a DenseNodeInfo<'a>> {
        self.info.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn set_test_user_sid(&mut self, user_sid: i32) {
        self.info.as_mut().expect("dense node metadata").user_sid = user_sid;
    }

    /// Returns the latitude coordinate in nanodegrees (10^-9).
    ///
    /// Scaling wraps on `i64` overflow rather than panicking or erroring; see
    /// "What is deliberately not checked" on block construction.
    #[inline]
    pub fn nano_lat(&self) -> i64 {
        self.lat_offset
            .wrapping_add(self.granularity.wrapping_mul(self.lat))
    }

    /// Returns the longitude in nanodegrees (10^-9). Wraps like
    /// [`nano_lat`](Self::nano_lat).
    #[inline]
    pub fn nano_lon(&self) -> i64 {
        self.lon_offset
            .wrapping_add(self.granularity.wrapping_mul(self.lon))
    }

    crate::impl_coordinate_conversions!();

    /// Returns an iterator over the tags of this node.
    pub fn tags(&self) -> DenseTagIter<'a> {
        DenseTagIter {
            block: self.block,
            cursor: Cursor::new(self.tag_bytes),
        }
    }

    /// Like [`tags`](Self::tags), but yields `Err` for a malformed varint or
    /// an out-of-range string index instead of silently ending the tag list.
    pub fn tags_checked(&self) -> CheckedDenseTagIter<'a> {
        CheckedDenseTagIter {
            block: self.block,
            cursor: Cursor::new(self.tag_bytes),
            failed: false,
        }
    }

    /// Returns an iterator over the tags of this node as raw index pairs.
    pub fn raw_tags(&self) -> DenseRawTagIter<'a> {
        DenseRawTagIter {
            cursor: Cursor::new(self.tag_bytes),
        }
    }
}

/// An iterator over dense nodes. It decodes the delta encoded values.
pub struct DenseNodeIter<'a> {
    block: &'a WireBlock<'static>,
    dids: PackedSint64Iter<'a>,
    cid: i64,
    dlats: PackedSint64Iter<'a>,
    clat: i64,
    dlons: PackedSint64Iter<'a>,
    clon: i64,
    /// Cursor over the raw packed int32 bytes of keys_vals.
    /// We scan forward through varints to find 0 delimiters between nodes' tags.
    kv_data: &'a [u8],
    kv_pos: usize,
    info_iter: Option<DenseNodeInfoIter<'a>>,
    granularity: i64,
    lat_offset: i64,
    lon_offset: i64,
}

impl<'a> DenseNodeIter<'a> {
    /// `dense` must come from a group of `block`, which block construction
    /// validated (`validate_groups_strict` ran `WireDenseInfo::parse` on this
    /// `denseinfo`). A `DenseInfo` parse failure is therefore a broken
    /// invariant and panics through `validated` rather than reading every
    /// node's metadata as absent.
    pub(super) fn new(
        block: &'a WireBlock<'static>,
        dense: WireDenseNodes<'a>,
    ) -> DenseNodeIter<'a> {
        let info_iter = dense
            .info_data
            .map(|data| validated(WireDenseInfo::parse(data), "DenseInfo"))
            .map(|info| DenseNodeInfoIter::new(block, info));
        DenseNodeIter {
            block,
            dids: PackedSint64Iter::new(dense.id_data),
            cid: 0,
            dlats: PackedSint64Iter::new(dense.lat_data),
            clat: 0,
            dlons: PackedSint64Iter::new(dense.lon_data),
            clon: 0,
            kv_data: dense.keys_vals_data,
            kv_pos: 0,
            info_iter,
            granularity: i64::from(block.granularity),
            lat_offset: block.lat_offset,
            lon_offset: block.lon_offset,
        }
    }

    pub(super) fn new_skip_metadata(
        block: &'a WireBlock<'static>,
        dense: WireDenseNodes<'a>,
    ) -> DenseNodeIter<'a> {
        DenseNodeIter {
            block,
            dids: PackedSint64Iter::new(dense.id_data),
            cid: 0,
            dlats: PackedSint64Iter::new(dense.lat_data),
            clat: 0,
            dlons: PackedSint64Iter::new(dense.lon_data),
            clon: 0,
            kv_data: dense.keys_vals_data,
            kv_pos: 0,
            info_iter: None,
            granularity: i64::from(block.granularity),
            lat_offset: block.lat_offset,
            lon_offset: block.lon_offset,
        }
    }

    pub(super) fn empty(block: &'a WireBlock<'static>) -> DenseNodeIter<'a> {
        DenseNodeIter {
            block,
            dids: PackedSint64Iter::empty(),
            cid: 0,
            dlats: PackedSint64Iter::empty(),
            clat: 0,
            dlons: PackedSint64Iter::empty(),
            clon: 0,
            kv_data: &[],
            kv_pos: 0,
            info_iter: None,
            granularity: i64::from(block.granularity),
            lat_offset: block.lat_offset,
            lon_offset: block.lon_offset,
        }
    }
}

impl<'a> Iterator for DenseNodeIter<'a> {
    type Item = DenseNode<'a>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        match (
            self.dids.next(),
            self.dlats.next(),
            self.dlons.next(),
            self.info_iter.as_mut().and_then(Iterator::next),
        ) {
            (Some(did), Some(dlat), Some(dlon), info) => {
                // Dense PBF deltas are modular; wrapping matches their signed
                // wire arithmetic in debug and release builds.
                self.cid = self.cid.wrapping_add(did);
                self.clat = self.clat.wrapping_add(dlat);
                self.clon = self.clon.wrapping_add(dlon);

                // Scan through packed int32 varints to find the 0 delimiter
                // that separates this node's tags from the next node's.
                //
                // PBF dense node tag encoding:
                //   The `keys_vals` array stores tags for ALL dense nodes in a block
                //   as a flat interleaved sequence of varints:
                //     [k1, v1, k2, v2, ..., 0, k1, v1, 0, 0, ...]
                //   Each node's tags are a run of (key_index, val_index) pairs,
                //   terminated by a single 0 varint delimiter.
                let tag_start = self.kv_pos;
                // `tag_end` trails `kv_pos`: it advances only over complete
                // (key, val) pairs, so it never includes the 0 delimiter.
                // Deriving it by
                // subtracting one byte from `kv_pos` instead - on the
                // assumption that the run always ended at a delimiter - chopped
                // the last byte of the final value varint whenever `keys_vals`
                // ran out without a trailing delimiter.
                let mut tag_end = self.kv_pos;
                let mut cursor = Cursor::new(&self.kv_data[self.kv_pos..]);

                // Scan forward, decoding varints until we hit a 0 key.
                // `validate_dense_keys_vals` walked these same varints at block
                // construction and refused a malformed one and a key with no
                // value, so neither read can fail here; a failure is a broken
                // invariant and panics (see `validated`) rather than ending
                // the node's tags early.
                while !cursor.is_empty() {
                    let before = cursor.remaining();
                    let key = validated(cursor.read_varint(), "DenseNodes keys_vals key");
                    if key == 0 {
                        // 0 delimiter found. Step kv_pos past the delimiter;
                        // tag_end stays before it.
                        self.kv_pos += before - cursor.remaining();
                        break;
                    }
                    // Skip the corresponding value varint.
                    validated(cursor.read_varint(), "DenseNodes keys_vals value");
                    self.kv_pos += before - cursor.remaining();
                    tag_end = self.kv_pos;
                }

                let tag_bytes = &self.kv_data[tag_start..tag_end];

                Some(DenseNode {
                    block: self.block,
                    id: self.cid,
                    lat: self.clat,
                    lon: self.clon,
                    tag_bytes,
                    info,
                    granularity: self.granularity,
                    lat_offset: self.lat_offset,
                    lon_offset: self.lon_offset,
                })
            }
            _ => None,
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.dids.size_hint()
    }
}

/// Optional metadata with non-geographic information about a dense node.
///
/// **Osmosis sentinel values:** Osmosis writes -1 for version and changeset
/// when metadata is absent. These fields use plain `i32`/`i64` (not `Option`)
/// because they are decoded from packed arrays in the dense node hot path -
/// changing to `Option` would add overhead to the tightest loop in the library.
/// The -1 sentinel is instead normalized at write/conversion boundaries
/// (see `dense_node_metadata`, `dense_node_raw_metadata`, sort's
/// `read_dense_node`, and the OSC writer's use of `claims`). Non-dense elements
/// normalize -1 → None directly in `WireInfo::parse`. See CORRECTNESS.md.
///
/// A version of 0 is not a claim either: it is how pbfhogg's `BlockBuilder`
/// and libosmium spell "no version", including the padding slot of a node
/// without metadata in a block whose other nodes carry it. Outputs that
/// print metadata go through [`claimed_version`](Self::claimed_version) and
/// the crate's `claims`, which read 0 and -1 accordingly.
#[derive(Clone, Debug)]
pub struct DenseNodeInfo<'a> {
    block: &'a WireBlock<'static>,
    version: i32,
    timestamp: i64,
    changeset: i64,
    uid: i32,
    user_sid: i32,
    visible: bool,
    date_granularity: i64,
}

impl<'a> DenseNodeInfo<'a> {
    /// Returns the version of this element.
    #[inline]
    pub fn version(&self) -> i32 {
        self.version
    }

    /// Returns the changeset id.
    #[inline]
    pub fn changeset(&self) -> i64 {
        self.changeset
    }

    /// Returns the user id.
    #[inline]
    pub fn uid(&self) -> i32 {
        self.uid
    }

    /// Returns the raw string table index for the user name.
    #[inline]
    pub fn raw_user_sid(&self) -> i32 {
        self.user_sid
    }

    /// Returns the user name.
    #[allow(clippy::cast_sign_loss)]
    pub fn user(&self) -> Result<&'a str> {
        if self.user_sid < 0 {
            return Err(crate::error::new_stringtable_index_out_of_bounds(0));
        }
        str_from_stringtable(self.block, self.user_sid as usize)
    }

    /// Returns the time stamp in milliseconds since the epoch.
    ///
    /// Scaling by `date_granularity` wraps on `i64` overflow, matching the
    /// modular delta reconstruction before it: a timestamp that far out is
    /// garbage in, garbage out, not a panic in debug and a wrap in release.
    #[inline]
    pub fn milli_timestamp(&self) -> i64 {
        self.timestamp.wrapping_mul(self.date_granularity)
    }

    /// Returns the visibility status of an element.
    // wontfix(name-is-has-bool): inherited from osmpbf public API
    #[inline]
    pub fn visible(&self) -> bool {
        self.visible
    }

    /// Returns true if the element was deleted.
    // wontfix(name-is-has-bool): inherited from osmpbf public API
    #[inline]
    pub fn deleted(&self) -> bool {
        !self.visible
    }

    /// Returns the version this node claims, or `None` when it claims none.
    ///
    /// OSM versions start at 1. A `DenseInfo` column cannot mark one slot
    /// absent, so writers spell "no version" in the value: 0 is what
    /// pbfhogg's `BlockBuilder` and libosmium write for a node without a
    /// version - including a node added without metadata to a block whose
    /// other nodes carry it - and -1 is the Osmosis sentinel. Neither is a
    /// claim. [`version`](Self::version) still returns the raw value.
    #[inline]
    pub fn claimed_version(&self) -> Option<i32> {
        (self.version >= 1).then_some(self.version)
    }

    /// The metadata fields this slot claims, for outputs that print them;
    /// `None` when it claims nothing.
    ///
    /// The packed columns decode a value for every field of every slot, so
    /// presence has to be read from the values:
    ///
    /// - version >= 1: every field is claimed as decoded, zeros included.
    /// - version 0: no version claim, and a zero in any other column is
    ///   indistinguishable from the padding a writer stores for a node
    ///   without metadata (pbfhogg's `BlockBuilder` and libosmium store all
    ///   zeros there). So each other field is claimed only by a value no
    ///   padding holds: timestamp non-zero, changeset and uid positive, a
    ///   user name other than the empty string, `visible` only when false.
    ///   The same tests as libosmium's `detect_available_metadata`. A real
    ///   versionless node with a zero field (an anonymous `uid` 0, say)
    ///   loses that field on output; on the wire it cannot be told apart.
    ///   Non-zero fields survive, which is what an apply-changes element
    ///   with a timestamp but no version, or `osmium cat --clean version`
    ///   output, carries.
    /// - version < 0: no metadata. -1 is the Osmosis "absent" sentinel, and
    ///   the conversions that re-encode a dense node drop such a slot too,
    ///   so printing and rewriting agree.
    ///
    /// Computed only where an output prints metadata; the dense iterator
    /// and the re-encoding conversions do not call it.
    pub(crate) fn claims(&self) -> Option<DenseClaims> {
        if self.version >= 1 {
            return Some(DenseClaims {
                version: Some(self.version),
                milli_timestamp: Some(self.milli_timestamp()),
                changeset: Some(self.changeset),
                uid: Some(self.uid),
                user: true,
                visible: Some(self.visible),
            });
        }
        if self.version < 0 {
            return None;
        }
        let milli_timestamp = self.milli_timestamp();
        Some(DenseClaims {
            version: None,
            milli_timestamp: (milli_timestamp != 0).then_some(milli_timestamp),
            changeset: (self.changeset > 0).then_some(self.changeset),
            uid: (self.uid > 0).then_some(self.uid),
            user: self.user_sid != 0,
            visible: (!self.visible).then_some(false),
        })
    }
}

/// The metadata fields one dense slot claims; see [`DenseNodeInfo::claims`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct DenseClaims {
    pub(crate) version: Option<i32>,
    pub(crate) milli_timestamp: Option<i64>,
    pub(crate) changeset: Option<i64>,
    pub(crate) uid: Option<i32>,
    /// Whether a user name is claimed. Resolve it with
    /// [`DenseNodeInfo::user`], which can fail on a corrupt string table;
    /// a bool keeps that lookup out of callers that only count.
    pub(crate) user: bool,
    pub(crate) visible: Option<bool>,
}

/// An iterator over dense nodes info. It decodes the delta encoded values.
///
/// `next` returns `None` as soon as any of the five core arrays runs out,
/// which on a short array would silently blank the metadata of every remaining
/// node in the group rather than reporting corruption. `Iterator::next` cannot
/// report anything, so the invariant is enforced one layer up instead:
/// `validate_dense_info_arrays` in `block.rs` runs at every `PrimitiveBlock`
/// constructor and rejects a `denseinfo` whose arrays are neither absent nor
/// exactly as long as the id array. By the time this iterator exists, the only
/// way to run out early is to have had nothing to begin with.
pub struct DenseNodeInfoIter<'a> {
    block: &'a WireBlock<'static>,
    versions: PackedInt32Iter<'a>,
    dtimestamps: PackedSint64Iter<'a>,
    ctimestamp: i64,
    dchangesets: PackedSint64Iter<'a>,
    cchangeset: i64,
    duids: PackedSint32Iter<'a>,
    cuid: i32,
    duser_sids: PackedSint32Iter<'a>,
    cuser_sid: i32,
    visible: PackedBoolIter<'a>,
    date_granularity: i64,
}

impl<'a> DenseNodeInfoIter<'a> {
    fn new(block: &'a WireBlock<'static>, info: WireDenseInfo<'a>) -> DenseNodeInfoIter<'a> {
        DenseNodeInfoIter {
            block,
            versions: PackedInt32Iter::new(info.version_data),
            dtimestamps: PackedSint64Iter::new(info.timestamp_data),
            ctimestamp: 0,
            dchangesets: PackedSint64Iter::new(info.changeset_data),
            cchangeset: 0,
            duids: PackedSint32Iter::new(info.uid_data),
            cuid: 0,
            duser_sids: PackedSint32Iter::new(info.user_sid_data),
            cuser_sid: 0,
            visible: PackedBoolIter::new(info.visible_data),
            date_granularity: i64::from(block.date_granularity),
        }
    }
}

impl<'a> Iterator for DenseNodeInfoIter<'a> {
    type Item = DenseNodeInfo<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let version = self.versions.next()?;
        let dtimestamp = self.dtimestamps.next()?;
        let dchangeset = self.dchangesets.next()?;
        let duid = self.duids.next()?;
        let duser_sid = self.duser_sids.next()?;
        let visible_opt = self.visible.next();

        // DenseInfo deltas are modular too, including the signed int32 arrays.
        self.ctimestamp = self.ctimestamp.wrapping_add(dtimestamp);
        self.cchangeset = self.cchangeset.wrapping_add(dchangeset);
        self.cuid = self.cuid.wrapping_add(duid);
        self.cuser_sid = self.cuser_sid.wrapping_add(duser_sid);

        Some(DenseNodeInfo {
            block: self.block,
            version,
            timestamp: self.ctimestamp,
            changeset: self.cchangeset,
            uid: self.cuid,
            user_sid: self.cuser_sid,
            visible: visible_opt.unwrap_or(true),
            date_granularity: self.date_granularity,
        })
    }
}

/// An iterator over the tags in a dense node.
#[derive(Clone)]
pub struct DenseTagIter<'a> {
    block: &'a WireBlock<'static>,
    cursor: Cursor<'a>,
}

#[allow(clippy::cast_possible_truncation)]
impl<'a> Iterator for DenseTagIter<'a> {
    type Item = (&'a str, &'a str);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.cursor.is_empty() {
            return None;
        }
        // The cursor covers only the complete (key, val) pairs `DenseNodeIter`
        // found in `keys_vals`, which block construction validated
        // (`validate_dense_keys_vals`), so neither read can fail; a failure is
        // a broken invariant and panics (see `validated`).
        // wontfix(type-result-fallible): an out-of-range string index is
        // different - construction does not resolve indices - and still ends
        // iteration, because Iterator constrains the return type. See
        // CORRECTNESS.md "Tag iterators end early on an out-of-range string
        // index".
        let key = validated(self.cursor.read_varint(), "DenseNodes keys_vals key") as usize;
        let val = validated(self.cursor.read_varint(), "DenseNodes keys_vals value") as usize;
        get_stringtable_key_value(self.block, Some(key), Some(val))
    }
}

/// The fallible sibling of [`DenseTagIter`]: yields `Err` for a malformed
/// varint or a key or value index past the string table, instead of ending
/// iteration. After an `Err` the iterator yields nothing more.
#[derive(Clone)]
pub struct CheckedDenseTagIter<'a> {
    block: &'a WireBlock<'static>,
    cursor: Cursor<'a>,
    failed: bool,
}

#[allow(clippy::cast_possible_truncation)]
impl<'a> Iterator for CheckedDenseTagIter<'a> {
    type Item = Result<(&'a str, &'a str)>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.cursor.is_empty() {
            return None;
        }
        let item = (|| {
            let key = self.cursor.read_varint()? as usize;
            let val = self.cursor.read_varint()? as usize;
            Ok((
                str_from_stringtable(self.block, key)?,
                str_from_stringtable(self.block, val)?,
            ))
        })();
        self.failed = item.is_err();
        Some(item)
    }
}

/// An iterator over the tags of a dense node as raw index pairs.
#[derive(Clone)]
pub struct DenseRawTagIter<'a> {
    cursor: Cursor<'a>,
}

impl Iterator for DenseRawTagIter<'_> {
    type Item = (i32, i32);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.cursor.is_empty() {
            return None;
        }
        // Neither read can fail, for the reason given in `DenseTagIter::next`;
        // the narrowing is lossless because construction refused any index
        // above `i32::MAX`.
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        let key = validated(self.cursor.read_varint(), "DenseNodes keys_vals key") as i32;
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        let val = validated(self.cursor.read_varint(), "DenseNodes keys_vals value") as i32;
        Some((key, val))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use crate::Element;
    use crate::PrimitiveBlock;
    use crate::write::block_builder::{BlockBuilder, Metadata};

    // MIN then MAX makes every second delta -1, so each running sum (ID,
    // timestamp, changeset, uid) has to wrap from MIN back to MAX.
    #[test]
    fn dense_node_deltas_decode_at_signed_boundaries() {
        let mut builder = BlockBuilder::new();
        let first = Metadata {
            version: 1,
            timestamp: i64::MIN,
            changeset: i64::MIN,
            uid: i32::MIN,
            user: "first",
            visible: true,
        };
        let next = Metadata {
            version: 1,
            timestamp: i64::MAX,
            changeset: i64::MAX,
            uid: i32::MAX,
            user: "next",
            visible: true,
        };
        builder.add_node(i64::MIN, 0, 0, [], Some(&first));
        builder.add_node(i64::MAX, 0, 0, [], Some(&next));

        let bytes = builder.take().unwrap().unwrap().to_vec();
        let block = PrimitiveBlock::from_vec(bytes).unwrap();
        let nodes: Vec<(i64, i64, i64, i32)> = block
            .elements()
            .map(|element| match element {
                Element::DenseNode(node) => {
                    let info = node.info().unwrap();
                    (node.id(), info.timestamp, info.changeset, info.uid())
                }
                _ => panic!("the fixture contains dense nodes only"),
            })
            .collect();

        assert_eq!(
            nodes,
            [
                (i64::MIN, i64::MIN, i64::MIN, i32::MIN),
                (i64::MAX, i64::MAX, i64::MAX, i32::MAX),
            ],
        );
    }
}
