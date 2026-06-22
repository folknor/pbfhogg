//! Iterate over the dense nodes in a `PrimitiveGroup`

use super::block::{get_stringtable_key_value, str_from_stringtable};
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

    /// Returns the latitude coordinate in nanodegrees (10^-9).
    #[inline]
    pub fn nano_lat(&self) -> i64 {
        self.lat_offset + self.granularity * self.lat
    }

    /// Returns the longitude in nanodegrees (10^-9).
    #[inline]
    pub fn nano_lon(&self) -> i64 {
        self.lon_offset + self.granularity * self.lon
    }

    crate::impl_coordinate_conversions!();

    /// Returns an iterator over the tags of this node.
    pub fn tags(&self) -> DenseTagIter<'a> {
        DenseTagIter {
            block: self.block,
            cursor: Cursor::new(self.tag_bytes),
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
    pub(crate) fn new(
        block: &'a WireBlock<'static>,
        dense: WireDenseNodes<'a>,
    ) -> DenseNodeIter<'a> {
        let info_iter = dense
            .info_data
            .and_then(|data| WireDenseInfo::parse(data).ok())
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

    pub(crate) fn new_skip_metadata(
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

    pub(crate) fn empty(block: &'a WireBlock<'static>) -> DenseNodeIter<'a> {
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
                self.cid += did;
                self.clat += dlat;
                self.clon += dlon;

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
                let mut cursor = Cursor::new(&self.kv_data[self.kv_pos..]);

                // Scan forward, decoding varints until we hit a 0 key.
                while !cursor.is_empty() {
                    let before = cursor.remaining();
                    match cursor.read_varint() {
                        Ok(0) => {
                            // 0 delimiter found. Record position past the delimiter.
                            let consumed = before - cursor.remaining();
                            self.kv_pos += consumed;
                            break;
                        }
                        Ok(_key) => {
                            // Skip the corresponding value varint
                            if cursor.read_varint().is_err() {
                                break;
                            }
                            let consumed = before - cursor.remaining();
                            self.kv_pos += consumed;
                        }
                        Err(_) => break,
                    }
                }

                // tag_bytes covers the key-value pairs for this node (before the 0 delimiter).
                // The kv_pos now points past the 0 delimiter for the next node.
                let tag_end = self.kv_pos.saturating_sub(if tag_start < self.kv_pos {
                    // Subtract the size of the 0 delimiter varint (always 1 byte: 0x00)
                    1
                } else {
                    0
                });
                let tag_bytes = &self.kv_data[tag_start..tag_end.min(self.kv_data.len())];

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
/// `read_dense_node`, and `stream_merge::convert_node`). Non-dense elements
/// normalize -1 → None directly in `WireInfo::parse`. See CORRECTNESS.md.
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
            return Err(crate::error::new_error(
                crate::error::ErrorKind::StringtableIndexOutOfBounds { index: 0 },
            ));
        }
        str_from_stringtable(self.block, self.user_sid as usize)
    }

    /// Returns the time stamp in milliseconds since the epoch.
    #[inline]
    pub fn milli_timestamp(&self) -> i64 {
        self.timestamp * self.date_granularity
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
}

/// An iterator over dense nodes info. It decodes the delta encoded values.
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

        self.ctimestamp += dtimestamp;
        self.cchangeset += dchangeset;
        self.cuid += duid;
        self.cuser_sid += duser_sid;

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
        // wontfix(type-result-fallible): Iterator trait constrains return type;
        // .ok() stops iteration on corrupt data rather than propagating errors.
        let key = self.cursor.read_varint().ok()? as usize;
        let val = self.cursor.read_varint().ok()? as usize;
        get_stringtable_key_value(self.block, Some(key), Some(val))
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
        // wontfix(type-result-fallible): same as DenseTagIter - Iterator constrains return type
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        let key = self.cursor.read_varint().ok()? as i32;
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        let val = self.cursor.read_varint().ok()? as i32;
        Some((key, val))
    }
}
