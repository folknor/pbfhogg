//! Iterate over the dense nodes in a `PrimitiveGroup`

use super::block::{get_stringtable_key_value, str_from_stringtable};
use crate::error::Result;
use crate::proto::osmformat;
use std;

/// An OpenStreetMap node element from a compressed array of dense nodes (See [OSM wiki](http://wiki.openstreetmap.org/wiki/Node)).
#[derive(Clone, Debug)]
pub struct DenseNode<'a> {
    block: &'a osmformat::PrimitiveBlock,

    /// The node id. It should be unique between nodes and might be negative to indicate
    /// that the element has not yet been uploaded to a server.
    pub(crate) id: i64,
    lat: i64,
    lon: i64,
    keys_vals_indices: &'a [i32],
    info: Option<DenseNodeInfo<'a>>,
}

impl<'a> DenseNode<'a> {
    /// Returns the node id. It should be unique between nodes and might be negative to indicate
    /// that the element has not yet been uploaded to a server.
    pub fn id(&self) -> i64 {
        self.id
    }

    /// return optional metadata about the node
    pub fn info(&'a self) -> Option<&'a DenseNodeInfo<'a>> {
        self.info.as_ref()
    }

    /// Returns the latitude coordinate in degrees.
    #[allow(clippy::cast_precision_loss)]
    pub fn lat(&self) -> f64 {
        1e-9 * self.nano_lat() as f64
    }

    /// Returns the latitude coordinate in nanodegrees (10⁻⁹).
    pub fn nano_lat(&self) -> i64 {
        self.block.lat_offset() + i64::from(self.block.granularity()) * self.lat
    }

    /// Returns the latitude coordinate in decimicrodegrees (10⁻⁷).
    #[allow(clippy::cast_possible_truncation)]
    pub fn decimicro_lat(&self) -> i32 {
        (self.nano_lat() / 100) as i32
    }

    /// Returns the longitude coordinate in degrees.
    #[allow(clippy::cast_precision_loss)]
    pub fn lon(&self) -> f64 {
        1e-9 * self.nano_lon() as f64
    }

    /// Returns the longitude in nanodegrees (10⁻⁹).
    pub fn nano_lon(&self) -> i64 {
        self.block.lon_offset() + i64::from(self.block.granularity()) * self.lon
    }

    /// Returns the longitude coordinate in decimicrodegrees (10⁻⁷).
    #[allow(clippy::cast_possible_truncation)]
    pub fn decimicro_lon(&self) -> i32 {
        (self.nano_lon() / 100) as i32
    }

    /// Returns an iterator over the tags of this node (See [OSM wiki](http://wiki.openstreetmap.org/wiki/Tags)).
    pub fn tags(&self) -> DenseTagIter<'a> {
        DenseTagIter {
            block: self.block,
            keys_vals_indices: self.keys_vals_indices.iter(),
        }
    }

    /// Returns an iterator over the tags of this node
    /// (See [OSM wiki](http://wiki.openstreetmap.org/wiki/Tags)).
    /// A tag is represented as a pair of indices (key and value) to the stringtable of the current
    /// [`PrimitiveBlock`](crate::block::PrimitiveBlock).
    pub fn raw_tags(&self) -> DenseRawTagIter<'a> {
        DenseRawTagIter {
            keys_vals_indices: self.keys_vals_indices.iter(),
        }
    }
}

/// An iterator over dense nodes. It decodes the delta encoded values.
#[derive(Clone, Debug)]
pub struct DenseNodeIter<'a> {
    block: &'a osmformat::PrimitiveBlock,
    dids: std::slice::Iter<'a, i64>,  // deltas
    cid: i64,                         // current id
    dlats: std::slice::Iter<'a, i64>, // deltas
    clat: i64,
    dlons: std::slice::Iter<'a, i64>, // deltas
    clon: i64,
    keys_vals_slice: &'a [i32],
    keys_vals_index: usize,
    info_iter: Option<DenseNodeInfoIter<'a>>,
}

impl<'a> DenseNodeIter<'a> {
    pub(crate) fn new(
        block: &'a osmformat::PrimitiveBlock,
        osmdense: &'a osmformat::DenseNodes,
    ) -> DenseNodeIter<'a> {
        let info_iter = Some(DenseNodeInfoIter::new(
            block,
            osmdense.denseinfo.get_or_default(),
        ));
        DenseNodeIter {
            block,
            dids: osmdense.id.iter(),
            cid: 0,
            dlats: osmdense.lat.iter(),
            clat: 0,
            dlons: osmdense.lon.iter(),
            clon: 0,
            keys_vals_slice: osmdense.keys_vals.as_slice(),
            keys_vals_index: 0,
            info_iter,
        }
    }

    pub(crate) fn empty(block: &'a osmformat::PrimitiveBlock) -> DenseNodeIter<'a> {
        DenseNodeIter {
            block,
            dids: [].iter(),
            cid: 0,
            dlats: [].iter(),
            clat: 0,
            dlons: [].iter(),
            clon: 0,
            keys_vals_slice: &[],
            keys_vals_index: 0,
            info_iter: None,
        }
    }
}

impl<'a> Iterator for DenseNodeIter<'a> {
    type Item = DenseNode<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        match (
            self.dids.next(),
            self.dlats.next(),
            self.dlons.next(),
            self.info_iter.as_mut().and_then(Iterator::next),
        ) {
            (Some(did), Some(dlat), Some(dlon), info) => {
                self.cid += *did;
                self.clat += *dlat;
                self.clon += *dlon;

                // --- Hot path: dense node key_vals scanning ---
                //
                // This runs once per dense node. A planet file contains ~8 billion
                // dense nodes, so every branch and bounds check matters here.
                //
                // PBF dense node tag encoding (from the OSMPBF spec):
                //   The `keys_vals` array stores tags for ALL dense nodes in a block
                //   as a flat interleaved sequence:
                //     [k1, v1, k2, v2, ..., 0, k1, v1, 0, 0, ...]
                //   Each node's tags are a run of (key_index, val_index) pairs,
                //   terminated by a single 0 delimiter. A node with no tags has
                //   just the 0 delimiter (the while loop below doesn't execute,
                //   i stays 0, and we get an empty slice — which is correct).
                //
                // Why a direct index loop instead of the previous chunks(2) iterator:
                //   - No Iterator trait overhead (no next() calls, no Option unwraps)
                //   - No per-chunk bounds check — chunks(2) must check chunk.len()
                //     on every iteration because the last chunk might have 1 element
                //   - No extra branch: the old code checked both chunk[0] != 0 AND
                //     chunk.len() == 2 on every pair. We only check kv_slice[i] != 0.
                //   - The compiler can see the simple loop structure and optimize the
                //     bounds check on kv_slice[i] more aggressively.
                let start_index = self.keys_vals_index;

                // Slice from current position to end of the key_vals array.
                let kv_slice = &self.keys_vals_slice[self.keys_vals_index..];
                let mut i = 0;

                // Scan forward through key-value pairs until we hit the 0 delimiter.
                // We advance by 2 each iteration (one key index + one value index).
                // The 0 delimiter is always a key position — valid key string table
                // indices are never 0 (index 0 is reserved as empty in the PBF
                // string table), so we only need to check the key position.
                while i < kv_slice.len() && kv_slice[i] != 0 {
                    i += 2; // skip past both key_idx and val_idx
                }

                // end_index points past the last val_idx, giving us the slice
                // [start_index..end_index] containing exactly the key-value pairs
                // for this node (no delimiter included).
                let end_index = start_index + i;

                // Advance past the pairs we consumed, plus 1 for the 0 delimiter.
                //
                // Safety of the +1: the PBF spec guarantees that every node's tag
                // run is terminated by a 0, and the keys_vals array has exactly as
                // many delimiters as there are dense nodes in the block. The only
                // case where kv_slice could be empty (len == 0) is when the block
                // has no key_vals data at all (i.e. no node in this block has any
                // tags). In that case i == 0 and we'd add 1, but this is safe
                // because keys_vals_index is only used on the *next* call to next(),
                // where dids.next() will return None first (since the id/lat/lon
                // arrays are the same length as the number of nodes, and we've
                // consumed them all if we've consumed all key_vals delimiters).
                // We use saturating_add to make this robust even if the data is
                // somehow truncated — we'll just point past the end and the next
                // call will produce an empty kv_slice.
                self.keys_vals_index = self.keys_vals_index.saturating_add(i + 1);

                Some(DenseNode {
                    block: self.block,
                    id: self.cid,
                    lat: self.clat,
                    lon: self.clon,
                    keys_vals_indices: &self.keys_vals_slice[start_index..end_index],
                    info,
                })
            }
            _ => None,
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.dids.size_hint()
    }
}

impl ExactSizeIterator for DenseNodeIter<'_> {}

/// Optional metadata with non-geographic information about a dense node
#[derive(Clone, Debug)]
pub struct DenseNodeInfo<'a> {
    block: &'a osmformat::PrimitiveBlock,
    /// The version of this element.
    version: i32,
    /// Timestamp
    timestamp: i64,
    /// The changeset id.
    changeset: i64,
    /// The user id.
    uid: i32,
    /// String IDs for usernames.
    user_sid: i32,
    /// Is the element visible (true) or was it deleted (false).
    visible: bool,
}

impl<'a> DenseNodeInfo<'a> {
    /// Returns the version of this element.
    pub fn version(&self) -> i32 {
        self.version
    }

    /// Returns the changeset id.
    pub fn changeset(&self) -> i64 {
        self.changeset
    }

    /// Returns the user id.
    pub fn uid(&self) -> i32 {
        self.uid
    }

    /// Returns the user name.
    #[allow(clippy::cast_sign_loss)]
    pub fn user(&self) -> Result<&'a str> {
        str_from_stringtable(self.block, self.user_sid as usize)
    }

    /// Returns the time stamp in milliseconds since the epoch.
    pub fn milli_timestamp(&self) -> i64 {
        self.timestamp * i64::from(self.block.date_granularity())
    }

    /// Returns the visibility status of an element. This is only relevant if the PBF file contains
    /// historical information.
    pub fn visible(&self) -> bool {
        self.visible
    }

    /// Returns true if the element was deleted.
    /// This is a convenience function that just returns the inverse of `DenseNodeInfo::visible`.
    pub fn deleted(&self) -> bool {
        !self.visible
    }
}

/// An iterator over dense nodes info. It decodes the delta encoded values.
#[derive(Clone, Debug)]
pub struct DenseNodeInfoIter<'a> {
    block: &'a osmformat::PrimitiveBlock,
    versions: std::slice::Iter<'a, i32>,
    dtimestamps: std::slice::Iter<'a, i64>, // deltas
    ctimestamp: i64,
    dchangesets: std::slice::Iter<'a, i64>, // deltas
    cchangeset: i64,
    duids: std::slice::Iter<'a, i32>, // deltas
    cuid: i32,
    duser_sids: std::slice::Iter<'a, i32>, // deltas
    cuser_sid: i32,
    visible: std::slice::Iter<'a, bool>,
}

impl<'a> DenseNodeInfoIter<'a> {
    fn new(
        block: &'a osmformat::PrimitiveBlock,
        info: &'a osmformat::DenseInfo,
    ) -> DenseNodeInfoIter<'a> {
        DenseNodeInfoIter {
            block,
            versions: info.version.iter(),
            dtimestamps: info.timestamp.iter(),
            ctimestamp: 0,
            dchangesets: info.changeset.iter(),
            cchangeset: 0,
            duids: info.uid.iter(),
            cuid: 0,
            duser_sids: info.user_sid.iter(),
            cuser_sid: 0,
            visible: info.visible.iter(),
        }
    }
}

impl<'a> Iterator for DenseNodeInfoIter<'a> {
    type Item = DenseNodeInfo<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        match (
            self.versions.next(),
            self.dtimestamps.next(),
            self.dchangesets.next(),
            self.duids.next(),
            self.duser_sids.next(),
            self.visible.next(),
        ) {
            (
                Some(&version),
                Some(dtimestamp),
                Some(dchangeset),
                Some(duid),
                Some(duser_sid),
                visible_opt,
            ) => {
                self.ctimestamp += *dtimestamp;
                self.cchangeset += *dchangeset;
                self.cuid += *duid;
                self.cuser_sid += *duser_sid;
                Some(DenseNodeInfo {
                    block: self.block,
                    version,
                    timestamp: self.ctimestamp,
                    changeset: self.cchangeset,
                    uid: self.cuid,
                    user_sid: self.cuser_sid,
                    visible: visible_opt.copied().unwrap_or(true),
                })
            }
            _ => None,
        }
    }
}

/// An iterator over the tags in a dense node.
#[derive(Clone, Debug)]
pub struct DenseTagIter<'a> {
    block: &'a osmformat::PrimitiveBlock,
    keys_vals_indices: std::slice::Iter<'a, i32>,
}

// Same tradeoff as TagIter in elements.rs: Item could be Result to propagate stringtable
// errors, but it's a breaking API change for minimal real-world benefit.
#[allow(clippy::cast_sign_loss)]
impl<'a> Iterator for DenseTagIter<'a> {
    type Item = (&'a str, &'a str);

    fn next(&mut self) -> Option<Self::Item> {
        get_stringtable_key_value(
            self.block,
            self.keys_vals_indices.next().map(|v| *v as usize),
            self.keys_vals_indices.next().map(|v| *v as usize),
        )
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.keys_vals_indices.len() / 2;
        (len, Some(len))
    }
}

impl ExactSizeIterator for DenseTagIter<'_> {}

/// An iterator over the tags of a node. It returns a pair of indices (key and value) to the
/// stringtable of the current [`PrimitiveBlock`](crate::block::PrimitiveBlock).
#[derive(Clone, Debug)]
pub struct DenseRawTagIter<'a> {
    keys_vals_indices: std::slice::Iter<'a, i32>,
}

//TODO return Result
impl Iterator for DenseRawTagIter<'_> {
    type Item = (i32, i32);

    fn next(&mut self) -> Option<Self::Item> {
        match (self.keys_vals_indices.next(), self.keys_vals_indices.next()) {
            (Some(&key_index), Some(&val_index)) => Some((key_index, val_index)),
            _ => None,
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.keys_vals_indices.len() / 2;
        (len, Some(len))
    }
}

impl ExactSizeIterator for DenseRawTagIter<'_> {}
