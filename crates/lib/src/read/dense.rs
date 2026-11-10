//! Iterate over the dense nodes in a `PrimitiveGroup`

use super::block::{get_stringtable_key_value, str_from_stringtable, validated};
use super::elements::zip_hint;
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
    pub fn info(&self) -> Option<&DenseNodeInfo<'a>> {
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
    /// validated (`validate_block` ran `WireDenseInfo::parse` on this
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
            .and_then(|info| {
                let columns = DenseColumns::of(&info);
                (!columns.is_empty()).then(|| DenseNodeInfoIter::new(block, info, columns))
            });
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
            (did, dlat, dlon, _) => {
                // Construction rejects id, lat and lon arrays of different
                // lengths (`validate_block` in `block.rs`), so all
                // three end together.
                super::block::arrays_end_together(
                    did.is_none() && dlat.is_none() && dlon.is_none(),
                    "dense id/lat/lon",
                );
                None
            }
        }
    }

    /// `info_iter` is left out: it is optional, has no hint of its own, and
    /// never limits iteration (a node without info still yields). The id
    /// array bounds it: a group whose only column is `visible` would yield
    /// past its end if driven alone, and here it never is.
    fn size_hint(&self) -> (usize, Option<usize>) {
        zip_hint(&[
            self.dids.size_hint(),
            self.dlats.size_hint(),
            self.dlons.size_hint(),
        ])
    }
}

/// Optional metadata with non-geographic information about a dense node.
///
/// **Osmosis sentinel values:** Osmosis writes -1 for version and changeset
/// when metadata is absent. These fields use plain `i32`/`i64` (not `Option`)
/// because they are decoded from packed arrays in the dense node hot path -
/// changing to `Option` would add overhead to the tightest loop in the library.
/// The -1 sentinel is instead read at the boundaries that print or rewrite
/// metadata (the crate's `claims`, and `dense_node_metadata`,
/// `dense_node_raw_metadata` and `read_dense_node`, which write a version of
/// -1 as 0). Non-dense elements normalize -1 to None directly in
/// `WireInfo::parse`. See CORRECTNESS.md.
///
/// A version of 0 is not a claim either: it is how pbfhogg's `BlockBuilder`
/// and libosmium spell "no version", including the padding slot of a node
/// without metadata in a block whose other nodes carry it. Outputs that
/// print metadata go through [`claimed_version`](Self::claimed_version) and
/// the crate's `claims`, which read 0 and -1 accordingly.
///
/// **Partial columns:** a `DenseInfo` may carry any subset of its six
/// columns (libosmium's `add_metadata=version+timestamp` writes two).
/// [`columns`](Self::columns) says which this node's group carries; a getter
/// of an absent column returns 0, as libosmium reads it, except
/// [`visible`](Self::visible), which returns `true`, and
/// [`user`](Self::user), which returns the empty name.
#[derive(Clone, Debug)]
pub struct DenseNodeInfo<'a> {
    block: &'a WireBlock<'static>,
    version: i32,
    timestamp: i64,
    changeset: i64,
    uid: i32,
    user_sid: i32,
    visible: bool,
    columns: DenseColumns,
    date_granularity: i64,
}

/// Which columns a dense group's `DenseInfo` carries, fixed for the group.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct DenseColumns(u8);

impl DenseColumns {
    const VERSION: u8 = 1;
    const TIMESTAMP: u8 = 1 << 1;
    const CHANGESET: u8 = 1 << 2;
    const UID: u8 = 1 << 3;
    const USER: u8 = 1 << 4;
    const VISIBLE: u8 = 1 << 5;
    const CORE: u8 = Self::VERSION | Self::TIMESTAMP | Self::CHANGESET | Self::UID | Self::USER;

    /// Every column, `visible` included: what `BlockBuilder` writes for a
    /// node added with complete metadata.
    pub const ALL: Self = Self(Self::CORE | Self::VISIBLE);

    /// The columns a parsed `DenseInfo` carries: those with any bytes.
    /// Validation made every non-empty column exactly as long as the id
    /// array, so non-empty is present.
    pub(crate) fn of(info: &WireDenseInfo<'_>) -> Self {
        let mut bits = 0;
        for (data, bit) in [
            (info.version_data, Self::VERSION),
            (info.timestamp_data, Self::TIMESTAMP),
            (info.changeset_data, Self::CHANGESET),
            (info.uid_data, Self::UID),
            (info.user_sid_data, Self::USER),
            (info.visible_data, Self::VISIBLE),
        ] {
            if !data.is_empty() {
                bits |= bit;
            }
        }
        Self(bits)
    }

    /// The columns a re-encode writes for a slot read with these columns:
    /// all six when the five core columns are present (the builder's
    /// complete-metadata shape, with absent `visible` written as the
    /// `true` it reads as), otherwise exactly these.
    #[must_use]
    pub(crate) fn for_reencode(self) -> Self {
        if self.0 & Self::CORE == Self::CORE {
            Self::ALL
        } else {
            self
        }
    }

    /// No column at all.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// All five of version, timestamp, changeset, uid and user.
    #[must_use]
    pub fn has_core(self) -> bool {
        self.0 & Self::CORE == Self::CORE
    }

    /// The version column is present.
    #[must_use]
    pub fn version(self) -> bool {
        self.0 & Self::VERSION != 0
    }

    /// The timestamp column is present.
    #[must_use]
    pub fn timestamp(self) -> bool {
        self.0 & Self::TIMESTAMP != 0
    }

    /// The changeset column is present.
    #[must_use]
    pub fn changeset(self) -> bool {
        self.0 & Self::CHANGESET != 0
    }

    /// The uid column is present.
    #[must_use]
    pub fn uid(self) -> bool {
        self.0 & Self::UID != 0
    }

    /// The user name column is present.
    #[must_use]
    pub fn user(self) -> bool {
        self.0 & Self::USER != 0
    }

    /// The visible column is present.
    #[must_use]
    pub fn visible(self) -> bool {
        self.0 & Self::VISIBLE != 0
    }
}

impl<'a> DenseNodeInfo<'a> {
    /// Returns the version of this element.
    #[inline]
    pub fn version(&self) -> i32 {
        self.version
    }

    /// The `DenseInfo` columns this node's group carries.
    #[inline]
    pub fn columns(&self) -> DenseColumns {
        self.columns
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

    /// Returns the user name; the empty name when the group has no user
    /// column, without a string-table lookup.
    ///
    /// A string index past the block's string table is
    /// `StringtableIndexOutOfBounds`; a negative one is a wire-format error,
    /// as for `RelMember::role`.
    #[allow(clippy::cast_sign_loss)]
    pub fn user(&self) -> Result<&'a str> {
        if !self.columns.user() {
            return Ok("");
        }
        if self.user_sid < 0 {
            return Err(crate::error::new_wire_error(
                "dense node info has negative user string index",
            ));
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
    /// A field whose column the group lacks is never claimed, except
    /// `visible` on a versioned slot (below). A present column decodes a
    /// value for every slot, so presence has to be read from the values:
    ///
    /// - version >= 1: every present field is claimed as decoded, zeros
    ///   included, except that a changeset of -1, the Osmosis sentinel, is
    ///   claimed as 0, as libosmium reads it. `visible` is claimed as
    ///   decoded, `true` when its column is absent.
    /// - version below 1 (0, the Osmosis -1, or any other negative value):
    ///   no version claim, and a zero in any other column is
    ///   indistinguishable from the padding a writer stores for a node
    ///   without metadata (pbfhogg's `BlockBuilder` and libosmium store all
    ///   zeros there). So each other field is claimed only by a value no
    ///   padding holds: timestamp non-zero, changeset and uid positive, a
    ///   user name other than the empty string, `visible` only when false.
    ///   The user name is judged by what it resolves to, not by its index:
    ///   index 0 (the empty string the format reserves) is unclaimed without
    ///   a lookup, and any other index is resolved and claimed only when the
    ///   name is non-empty, so the claim does not depend on where a writer
    ///   put the empty string in its table.
    ///   The timestamp, changeset, uid and user tests are the value tests of
    ///   libosmium's `detect_available_metadata` (`visible` is not among
    ///   them). A real
    ///   versionless node with a zero field (an anonymous `uid` 0, say)
    ///   loses that field on output; on the wire it cannot be told apart.
    ///   Non-zero fields survive, which is what an apply-changes element
    ///   with a timestamp but no version, or `osmium cat --clean version`
    ///   output, carries.
    ///   A negative version used to drop the whole slot; it no longer does,
    ///   as libosmium reads -1 as "no version" and keeps the other fields.
    ///   An absent column decodes as 0 (`visible` as true), which no value
    ///   test claims, so the column rule needs no extra check here.
    ///
    /// The re-encoding conversions (`dense_node_metadata` and its kin) write
    /// what this reads back the same, apart from the sub-second part of a
    /// timestamp, which they truncate to whole seconds.
    ///
    /// Always `Some` today; the `Option` stays for the callers' shape.
    /// Computed only where an output reads metadata; the dense iterator
    /// and the re-encoding conversions do not call it.
    #[allow(clippy::unnecessary_wraps)]
    pub(crate) fn claims(&self) -> Option<DenseClaims<'a>> {
        if self.version >= 1 {
            let columns = self.columns;
            return Some(DenseClaims {
                // An absent version column reads 0, so this one is present.
                version: Some(self.version),
                milli_timestamp: columns.timestamp().then(|| self.milli_timestamp()),
                // libosmium reads the Osmosis -1 as 0, and the re-encoding
                // conversions write it so.
                changeset: columns.changeset().then_some(if self.changeset == -1 {
                    0
                } else {
                    self.changeset
                }),
                uid: columns.uid().then_some(self.uid),
                user: columns.user().then(|| self.user()),
                visible: Some(self.visible),
            });
        }
        let milli_timestamp = self.milli_timestamp();
        let user = match self.user_sid {
            0 => None,
            _ => match self.user() {
                Ok("") => None,
                resolved => Some(resolved),
            },
        };
        Some(DenseClaims {
            version: None,
            milli_timestamp: (milli_timestamp != 0).then_some(milli_timestamp),
            changeset: (self.changeset > 0).then_some(self.changeset),
            uid: (self.uid > 0).then_some(self.uid),
            user,
            visible: (!self.visible).then_some(false),
        })
    }
}

/// The metadata fields one dense slot claims; see [`DenseNodeInfo::claims`].
#[derive(Clone, Debug)]
pub(crate) struct DenseClaims<'a> {
    pub(crate) version: Option<i32>,
    pub(crate) milli_timestamp: Option<i64>,
    pub(crate) changeset: Option<i64>,
    pub(crate) uid: Option<i32>,
    /// The claimed user name, already resolved. A bad string index is an
    /// `Err` value here, not a failed claim: each output decides what a
    /// corrupt name means for it (refuse, or print a marker).
    pub(crate) user: Option<Result<&'a str>>,
    pub(crate) visible: Option<bool>,
}

/// An iterator over dense nodes info. It decodes the delta encoded values.
///
/// `next` returns `None` as soon as any present core column runs out, which
/// on a short array would silently blank the metadata of every remaining
/// node in the group rather than reporting corruption. `Iterator::next` cannot
/// report anything, so the invariant is enforced one layer up instead:
/// `validate_dense_info_arrays` in `block.rs` runs at every `PrimitiveBlock`
/// constructor and rejects a `denseinfo` whose arrays are neither absent nor
/// exactly as long as the id array.
///
/// Column presence is chosen once per group ([`DenseColumns`]). A column
/// read keeps the single `None` test of a complete group; only on `None` does
/// the cold `absent` step tell the end of a present column (stop) from a
/// column the group lacks (read 0). So a complete-column group does no more
/// work per node than before partial columns were read.
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
    columns: DenseColumns,
    date_granularity: i64,
}

/// The value of a column read that returned `None`: the end of iteration
/// (`None`) when the group carries `column`, else the 0 an absent column
/// reads as. Cold: on a complete group it runs once, at the end.
#[cold]
#[inline(never)]
fn absent<T: Default>(columns: DenseColumns, column: fn(DenseColumns) -> bool) -> Option<T> {
    if column(columns) {
        None
    } else {
        Some(T::default())
    }
}

impl<'a> DenseNodeInfoIter<'a> {
    fn new(
        block: &'a WireBlock<'static>,
        info: WireDenseInfo<'a>,
        columns: DenseColumns,
    ) -> DenseNodeInfoIter<'a> {
        DenseNodeInfoIter {
            columns,
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
        let columns = self.columns;
        let version = match self.versions.next() {
            Some(v) => v,
            None => absent(columns, DenseColumns::version)?,
        };
        let dtimestamp = match self.dtimestamps.next() {
            Some(v) => v,
            None => absent(columns, DenseColumns::timestamp)?,
        };
        let dchangeset = match self.dchangesets.next() {
            Some(v) => v,
            None => absent(columns, DenseColumns::changeset)?,
        };
        let duid = match self.duids.next() {
            Some(v) => v,
            None => absent(columns, DenseColumns::uid)?,
        };
        let duser_sid = match self.duser_sids.next() {
            Some(v) => v,
            None => absent(columns, DenseColumns::user)?,
        };
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
            columns,
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
        // index". Emptying the cursor on that branch keeps the end final
        // (the iterator is fused) at no cost on the success path.
        let key = validated(self.cursor.read_varint(), "DenseNodes keys_vals key") as usize;
        let val = validated(self.cursor.read_varint(), "DenseNodes keys_vals value") as usize;
        let pair = get_stringtable_key_value(self.block, Some(key), Some(val));
        if pair.is_none() {
            self.cursor = Cursor::new(&[]);
        }
        pair
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        // Each remaining key/value pair occupies at least two bytes.
        (0, Some(self.cursor.remaining() / 2))
    }
}

impl std::iter::FusedIterator for DenseTagIter<'_> {}

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

    fn size_hint(&self) -> (usize, Option<usize>) {
        if self.failed {
            return (0, Some(0));
        }
        // Each remaining key/value pair occupies at least two bytes.
        (0, Some(self.cursor.remaining() / 2))
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

    fn size_hint(&self) -> (usize, Option<usize>) {
        // Each remaining key/value pair occupies at least two bytes.
        (0, Some(self.cursor.remaining() / 2))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::Element;
    use crate::PrimitiveBlock;
    use crate::write::block_builder::{BlockBuilder, Metadata};

    #[test]
    fn dense_tag_size_hint() {
        let mut builder = BlockBuilder::new();
        builder.add_node(1, 0, 0, [], None);
        let bytes = builder.take().unwrap().unwrap().to_vec();
        let block = PrimitiveBlock::from_vec(bytes).unwrap();
        let Element::DenseNode(node) = block.elements().next().unwrap() else {
            panic!("fixture contains a dense node");
        };
        // A two-byte varint makes the byte bound larger than the pair count.
        let data = [0x80, 0, 0x80, 0, 0, 0];
        let mut tags = DenseTagIter {
            block: node.block,
            cursor: Cursor::new(&data),
        };
        let mut checked = CheckedDenseTagIter {
            block: node.block,
            cursor: Cursor::new(&data),
            failed: false,
        };
        let mut raw = DenseRawTagIter {
            cursor: Cursor::new(&data),
        };
        for upper in [3, 1, 0] {
            assert_eq!(tags.size_hint(), (0, Some(upper)));
            assert_eq!(checked.size_hint(), (0, Some(upper)));
            assert_eq!(raw.size_hint(), (0, Some(upper)));
            if upper > 0 {
                assert!(tags.next().is_some());
                assert!(checked.next().unwrap().is_ok());
                assert!(raw.next().is_some());
            }
        }
        assert!(tags.next().is_none());
        assert!(checked.next().is_none());
        assert!(raw.next().is_none());

        let mut tags = DenseTagIter {
            block: node.block,
            cursor: Cursor::new(&[127, 0, 0, 0]),
        };
        assert!(tags.next().is_none());
        assert_eq!(tags.size_hint(), (0, Some(0)));
        for data in [&[127, 0, 0, 0][..], &[0x80, 0x80, 0x80, 0x80][..]] {
            let mut checked = CheckedDenseTagIter {
                block: node.block,
                cursor: Cursor::new(data),
                failed: false,
            };
            assert_eq!(checked.size_hint(), (0, Some(2)));
            assert!(checked.next().unwrap().is_err());
            assert_eq!(checked.size_hint(), (0, Some(0)));
            assert!(checked.next().is_none());
        }
    }

    fn meta(version: i32, timestamp: i64, changeset: i64, uid: i32, user: &str) -> Metadata<'_> {
        Metadata {
            version,
            timestamp,
            changeset,
            uid,
            user,
            visible: true,
        }
    }

    fn dense_infos(block: &PrimitiveBlock) -> Vec<(i64, Option<DenseNodeInfo<'_>>)> {
        block
            .elements()
            .map(|element| match element {
                Element::DenseNode(node) => (node.id(), node.info().cloned()),
                _ => panic!("the fixture contains dense nodes only"),
            })
            .collect()
    }

    const VERSION_TIMESTAMP: DenseColumns =
        DenseColumns(DenseColumns::VERSION | DenseColumns::TIMESTAMP);

    /// libosmium's `add_metadata=version+timestamp` shape: a group carrying
    /// two columns reads both, reads the others as absent (0, visible true,
    /// empty user without a lookup), claims only what it carries, and
    /// re-encodes dense with the same two columns, not with invented ones.
    #[test]
    fn partial_columns_read_claim_and_reencode_without_inventing_columns() {
        use crate::block_builder::DenseMetadata;
        let mut builder = BlockBuilder::new();
        for (id, version, ts) in [(1, 3, 100), (2, 4, 200)] {
            builder.add_node_dense(
                id,
                0,
                0,
                [],
                Some(&DenseMetadata {
                    meta: Metadata {
                        visible: false,
                        ..meta(version, ts, 9, 7, "u")
                    },
                    columns: VERSION_TIMESTAMP,
                }),
            );
        }
        // A complete node after them goes into a group of its own.
        builder.add_node(3, 0, 0, [], Some(&meta(5, 300, 11, 13, "v")));
        let bytes = builder.take().unwrap().unwrap().to_vec();
        let block = PrimitiveBlock::from_vec(bytes).unwrap();

        let infos = dense_infos(&block);
        assert_eq!(infos.len(), 3);
        for ((id, info), (want_version, want_ts)) in infos[..2].iter().zip([(3, 100), (4, 200)]) {
            let info = info
                .as_ref()
                .unwrap_or_else(|| panic!("node {id} has info"));
            assert_eq!(info.columns(), VERSION_TIMESTAMP);
            assert_eq!(info.version(), want_version);
            assert_eq!(info.milli_timestamp(), want_ts * 1000);
            assert_eq!((info.changeset(), info.uid()), (0, 0));
            assert_eq!(info.user().unwrap(), "");
            assert!(info.visible(), "absent visible reads true");
            let claims = info.claims().unwrap();
            assert_eq!(claims.version, Some(want_version));
            assert_eq!(claims.milli_timestamp, Some(want_ts * 1000));
            assert_eq!((claims.changeset, claims.uid), (None, None));
            assert!(claims.user.is_none());
            assert_eq!(claims.visible, Some(true));
        }
        let complete = infos[2].1.as_ref().unwrap();
        assert_eq!(complete.columns(), DenseColumns::ALL);
        assert_eq!((complete.version(), complete.uid()), (5, 13));

        // Re-encode every node through the conversion the commands use.
        let mut out = BlockBuilder::new();
        for element in block.elements() {
            let Element::DenseNode(node) = element else {
                panic!("dense only");
            };
            let meta = crate::owned::dense_node_metadata(&node).unwrap();
            out.add_node_dense(node.id(), 0, 0, [], meta.as_ref());
        }
        let bytes = out.take().unwrap().unwrap().to_vec();
        let back = PrimitiveBlock::from_vec(bytes).unwrap();
        let back = dense_infos(&back);
        let columns: Vec<_> = back
            .iter()
            .map(|(_, i)| i.as_ref().unwrap().columns())
            .collect();
        assert_eq!(
            columns,
            [VERSION_TIMESTAMP, VERSION_TIMESTAMP, DenseColumns::ALL]
        );
        let values: Vec<_> = back
            .iter()
            .map(|(id, i)| {
                let i = i.as_ref().unwrap();
                (*id, i.version(), i.milli_timestamp())
            })
            .collect();
        assert_eq!(values, [(1, 3, 100_000), (2, 4, 200_000), (3, 5, 300_000)]);
    }

    /// A history file written without metadata carries `visible` alone.
    #[test]
    fn a_visible_only_group_reads_and_claims_only_visibility() {
        use crate::block_builder::DenseMetadata;
        let mut builder = BlockBuilder::new();
        for (id, visible) in [(1, false), (2, true)] {
            builder.add_node_dense(
                id,
                0,
                0,
                [],
                Some(&DenseMetadata {
                    meta: Metadata {
                        visible,
                        ..meta(0, 0, 0, 0, "")
                    },
                    columns: DenseColumns(DenseColumns::VISIBLE),
                }),
            );
        }
        let bytes = builder.take().unwrap().unwrap().to_vec();
        let block = PrimitiveBlock::from_vec(bytes).unwrap();
        let infos = dense_infos(&block);
        assert_eq!(infos.len(), 2, "the id array bounds a visible-only group");
        let first = infos[0].1.as_ref().unwrap();
        assert!(!first.visible());
        let claims = first.claims().unwrap();
        assert_eq!(claims.version, None);
        assert_eq!(claims.visible, Some(false));
        assert!(infos[1].1.as_ref().unwrap().visible());
    }

    /// The Osmosis -1 version no longer blanks the slot: printing claims its
    /// real fields by value, and the re-encode keeps them with version 0 and
    /// the -1 changeset as 0.
    #[test]
    fn a_minus_one_version_keeps_its_other_fields() {
        let mut builder = BlockBuilder::new();
        builder.add_node(1, 0, 0, [], Some(&meta(-1, 5, -1, 9, "bob")));
        let bytes = builder.take().unwrap().unwrap().to_vec();
        let block = PrimitiveBlock::from_vec(bytes).unwrap();
        let Some(Element::DenseNode(node)) = block.elements().next() else {
            panic!("dense node");
        };
        let claims = node.info().unwrap().claims().unwrap();
        assert_eq!(claims.version, None);
        assert_eq!(claims.milli_timestamp, Some(5000));
        assert_eq!((claims.changeset, claims.uid), (None, Some(9)));
        assert_eq!(claims.user.unwrap().unwrap(), "bob");

        let converted = crate::owned::dense_node_metadata(&node).unwrap().unwrap();
        assert_eq!(converted.meta, meta(0, 5, 0, 9, "bob"));
        assert_eq!(converted.columns, DenseColumns::ALL);
    }

    /// A versioned slot's -1 changeset claims 0, which is what re-encoding
    /// writes, so the output claims what the input did.
    #[test]
    fn a_versioned_minus_one_changeset_claims_zero() {
        let mut builder = BlockBuilder::new();
        builder.add_node(1, 0, 0, [], Some(&meta(2, 5, -1, 9, "bob")));
        let bytes = builder.take().unwrap().unwrap().to_vec();
        let block = PrimitiveBlock::from_vec(bytes).unwrap();
        let Some(Element::DenseNode(node)) = block.elements().next() else {
            panic!("dense node");
        };
        assert_eq!(node.info().unwrap().claims().unwrap().changeset, Some(0));
        let converted = crate::owned::dense_node_metadata(&node).unwrap().unwrap();
        assert_eq!(converted.meta.changeset, 0);
    }

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
