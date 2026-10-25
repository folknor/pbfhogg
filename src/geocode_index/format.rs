//! On-disk format definitions for the reverse geocoding index.
//!
//! All records use little-endian byte order and manual serialization (no
//! `#[repr(C)]` transmutation) to avoid alignment padding issues.
//!
//! This module is the authoritative on-disk format definition: the record
//! layouts below (magic bytes, header, and the per-file records) are the
//! format. It also owns how records are found: a whole record file, a
//! fixed-count span of records at a byte offset, and the length-prefixed
//! entry lists, all read through [`Table`], whose list framing is
//! [`ListCount`] and [`list_byte_len`].

use crate::coord_fmt::e7_to_deg;

/// Magic bytes for the index header: `GIDX`.
pub const HEADER_MAGIC: [u8; 4] = *b"GIDX";

/// Current format version. Version 3 widens `AdminCell.entries_offset` and
/// `AdminPolygon.vertex_offset` from u32 to u64. Readers deliberately reject
/// older versions: existing indexes must be rebuilt.
pub const FORMAT_VERSION: u32 = 3;

/// S2's on-disk cell level limit, independent of build sampling budgets.
pub const S2_MAX_LEVEL: u8 = 30;

/// Version 3 reserves level 11 for postal boundaries. Real administrative
/// levels above 10 are deliberately unsupported: widening acceptance requires
/// a distinct boundary kind and a format migration, not reuse of this slot.
pub const POSTAL_BOUNDARY_LEVEL: u8 = 11;
pub const MAX_ADMIN_LEVEL: u8 = 10;
pub const ADMIN_LEVEL_SLOTS: usize = POSTAL_BOUNDARY_LEVEL as usize + 1;
const _: () = assert!(MAX_ADMIN_LEVEL < POSTAL_BOUNDARY_LEVEL);

/// Wire values retained from version 3; naming them does not change bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum InterpolationType {
    All = 0,
    Even = 1,
    Odd = 2,
}

impl TryFrom<u8> for InterpolationType {
    type Error = FormatError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::All),
            1 => Ok(Self::Even),
            2 => Ok(Self::Odd),
            _ => Err(FormatError::InvalidMetadata(format!(
                "unknown interpolation type {value}"
            ))),
        }
    }
}

/// Sentinel value for "no data" in u64 offset fields.
///
/// Every optional offset in format v3 is u64. Admin offsets are mandatory, so
/// they never use this sentinel.
pub const NO_DATA_U64: u64 = u64::MAX;

/// Interior cell hint flag (high bit on a u32 polygon index).
pub const INTERIOR_FLAG: u32 = 0x8000_0000;

/// Mask to extract the polygon index from an interior-flagged entry.
pub const INDEX_MASK: u32 = 0x7FFF_FFFF;

/// Header size on disk (128 bytes: 64 used + 64 reserved).
pub const HEADER_SIZE: usize = 128;

// ---------------------------------------------------------------------------
// Header (128 bytes)
// ---------------------------------------------------------------------------

/// Index header. Fixed-size, read once at `Reader::open`.
#[derive(Debug, Clone)]
pub struct Header {
    pub format_version: u32,
    pub street_cell_level: u8,
    pub coarse_cell_level: u8,
    pub admin_cell_level: u8,
    /// Base local-boundary simplification budget, not a stored-record limit.
    /// Country/region budgets are multiples of this value; readers need only
    /// each polygon's actual vertex_count. Retained for v3 metadata compatibility.
    pub max_admin_vertices: u16,
    pub fine_search_radius_m: f32,
    pub coarse_search_radius_m: f32,
    pub replication_sequence: u32,
    pub replication_timestamp: u64,
    pub addr_point_count: u32,
    pub street_way_count: u32,
    pub interp_way_count: u32,
    pub admin_polygon_count: u32,
    pub geo_cell_count: u32,
    pub coarse_cell_count: u32,
    pub admin_cell_count: u32,
}

impl Header {
    pub fn to_bytes(&self) -> [u8; HEADER_SIZE] {
        let mut buf = [0u8; HEADER_SIZE];
        buf[0..4].copy_from_slice(&HEADER_MAGIC);
        buf[4..8].copy_from_slice(&self.format_version.to_le_bytes());
        buf[8] = self.street_cell_level;
        buf[9] = self.coarse_cell_level;
        buf[10] = self.admin_cell_level;
        // buf[11] reserved
        buf[12..14].copy_from_slice(&self.max_admin_vertices.to_le_bytes());
        // buf[14..16] reserved
        buf[16..20].copy_from_slice(&self.fine_search_radius_m.to_le_bytes());
        buf[20..24].copy_from_slice(&self.coarse_search_radius_m.to_le_bytes());
        buf[24..28].copy_from_slice(&self.replication_sequence.to_le_bytes());
        buf[28..36].copy_from_slice(&self.replication_timestamp.to_le_bytes());
        buf[36..40].copy_from_slice(&self.addr_point_count.to_le_bytes());
        buf[40..44].copy_from_slice(&self.street_way_count.to_le_bytes());
        buf[44..48].copy_from_slice(&self.interp_way_count.to_le_bytes());
        buf[48..52].copy_from_slice(&self.admin_polygon_count.to_le_bytes());
        buf[52..56].copy_from_slice(&self.geo_cell_count.to_le_bytes());
        buf[56..60].copy_from_slice(&self.coarse_cell_count.to_le_bytes());
        buf[60..64].copy_from_slice(&self.admin_cell_count.to_le_bytes());
        // buf[64..128] reserved (already zero)
        buf
    }

    pub fn from_bytes(buf: &[u8; HEADER_SIZE]) -> Result<Self, FormatError> {
        if buf[0..4] != HEADER_MAGIC {
            return Err(FormatError::BadMagic);
        }
        let version = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
        if version != FORMAT_VERSION {
            return Err(FormatError::UnsupportedVersion(version));
        }
        let street = buf[8];
        let coarse = buf[9];
        let admin = buf[10];
        // 30 is the S2 format limit, deliberately looser than the builder's
        // own `MAX_BUILD_S2_LEVEL`. The builder's cap comes from its segment
        // sampling budget - a cost of *writing* an index, not a property of
        // one - so an index written at a higher level by some other producer
        // stays readable. Reading does no cell covering.
        if street > S2_MAX_LEVEL || coarse > S2_MAX_LEVEL || admin > S2_MAX_LEVEL {
            return Err(FormatError::InvalidMetadata(format!(
                "S2 cell levels must be in 0..={S2_MAX_LEVEL}, got street={street}, coarse={coarse}, admin={admin}"
            )));
        }
        if coarse >= street {
            return Err(FormatError::InvalidMetadata(format!(
                "coarse S2 level ({coarse}) must be less than street S2 level ({street})"
            )));
        }
        let fine_search_radius_m = f32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]);
        let coarse_search_radius_m = f32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]);
        validate_radius("search radius", fine_search_radius_m)
            .map_err(|e| FormatError::InvalidMetadata(e.to_string()))?;
        validate_radius("coarse search radius", coarse_search_radius_m)
            .map_err(|e| FormatError::InvalidMetadata(e.to_string()))?;
        Ok(Self {
            format_version: version,
            street_cell_level: street,
            coarse_cell_level: coarse,
            admin_cell_level: admin,
            max_admin_vertices: u16::from_le_bytes([buf[12], buf[13]]),
            fine_search_radius_m,
            coarse_search_radius_m,
            replication_sequence: u32::from_le_bytes([buf[24], buf[25], buf[26], buf[27]]),
            replication_timestamp: u64::from_le_bytes([
                buf[28], buf[29], buf[30], buf[31], buf[32], buf[33], buf[34], buf[35],
            ]),
            addr_point_count: u32::from_le_bytes([buf[36], buf[37], buf[38], buf[39]]),
            street_way_count: u32::from_le_bytes([buf[40], buf[41], buf[42], buf[43]]),
            interp_way_count: u32::from_le_bytes([buf[44], buf[45], buf[46], buf[47]]),
            admin_polygon_count: u32::from_le_bytes([buf[48], buf[49], buf[50], buf[51]]),
            geo_cell_count: u32::from_le_bytes([buf[52], buf[53], buf[54], buf[55]]),
            coarse_cell_count: u32::from_le_bytes([buf[56], buf[57], buf[58], buf[59]]),
            admin_cell_count: u32::from_le_bytes([buf[60], buf[61], buf[62], buf[63]]),
        })
    }
}

// ---------------------------------------------------------------------------
// Fixed-size records
// ---------------------------------------------------------------------------

/// A fixed-size little-endian record of one of the index files.
///
/// Every record type is defined below, and only there: the trait is sealed.
/// `SIZE` is nonzero, and `read` succeeds for every byte slice of exactly
/// `SIZE` bytes - validating field *values* (an interpolation type, an
/// offset into another file) is the reader's job, not the codec's.
pub trait Record: Copy + sealed::Sealed {
    /// Bytes per record on disk.
    const SIZE: usize;

    /// Decode one record; `None` exactly when `bytes.len() != SIZE`.
    fn read(bytes: &[u8]) -> Option<Self>;
}

mod sealed {
    pub trait Sealed {}
}

/// A little-endian primitive a record field is stored as. Its width is the
/// whole of a field's layout: a record's field offsets and `SIZE` are sums of
/// these widths, in field order.
trait WireField: Copy {
    const WIDTH: usize;
    fn get(buf: &[u8], at: usize) -> Self;
    fn put(self, buf: &mut [u8], at: usize);
}

macro_rules! wire_field {
    ($($ty:ty),*) => {$(
        impl WireField for $ty {
            const WIDTH: usize = std::mem::size_of::<$ty>();

            #[inline(always)]
            fn get(buf: &[u8], at: usize) -> Self {
                let mut bytes = [0u8; std::mem::size_of::<$ty>()];
                bytes.copy_from_slice(&buf[at..at + Self::WIDTH]);
                <$ty>::from_le_bytes(bytes)
            }

            #[inline(always)]
            fn put(self, buf: &mut [u8], at: usize) {
                buf[at..at + Self::WIDTH].copy_from_slice(&self.to_le_bytes());
            }
        }
    )*};
}

wire_field!(u8, u16, u32, u64, i32, f32);

/// Define a record from its field list, which is its on-disk layout: fields
/// are stored in the order listed, each at the width of its type, followed by
/// `reserved` zero bytes that writers emit and readers ignore. `SIZE`, every
/// field offset, `to_bytes` and `from_bytes` all derive from that one list.
macro_rules! record {
    (
        $(#[$meta:meta])*
        pub struct $name:ident {
            $( $(#[$field_meta:meta])* pub $field:ident : $ty:ty, )+
        }
        $( reserved = $reserved:literal; )?
    ) => {
        $(#[$meta])*
        pub struct $name {
            $( $(#[$field_meta])* pub $field: $ty, )+
        }

        impl $name {
            /// Bytes per record on disk.
            pub const SIZE: usize = 0 $( + <$ty as WireField>::WIDTH )+ $( + $reserved )?;

            #[inline]
            #[allow(unused_assignments)]
            pub fn to_bytes(&self) -> [u8; Self::SIZE] {
                let mut buf = [0u8; Self::SIZE];
                let mut at = 0;
                $(
                    WireField::put(self.$field, &mut buf, at);
                    at += <$ty as WireField>::WIDTH;
                )+
                buf
            }

            #[inline]
            #[allow(unused_assignments)]
            pub fn from_bytes(buf: &[u8; Self::SIZE]) -> Self {
                let mut at = 0;
                $(
                    let $field = <$ty as WireField>::get(buf, at);
                    at += <$ty as WireField>::WIDTH;
                )+
                Self { $( $field, )+ }
            }
        }

        impl sealed::Sealed for $name {}

        impl Record for $name {
            const SIZE: usize = $name::SIZE;

            #[inline]
            fn read(bytes: &[u8]) -> Option<Self> {
                bytes.try_into().ok().map(Self::from_bytes)
            }
        }
    };
}

record! {
    /// Merged street-level cell index entry (`geo_cells.bin`,
    /// `coarse_geo_cells.bin`), sorted by `cell_id`.
    #[derive(Debug, Clone, Copy)]
    pub struct GeoCell {
        pub cell_id: u64,
        /// Byte offset of this cell's list in street_entries.bin.
        pub street_offset: u64,
        /// Byte offset of this cell's list in addr_entries.bin.
        pub addr_offset: u64,
        /// Byte offset of this cell's list in interp_entries.bin.
        pub interp_offset: u64,
    }
}

record! {
    /// A reference to a specific segment within a way: the item of
    /// street_entries.bin and interp_entries.bin lists.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct SegmentRef {
        pub way_index: u32,
        pub segment_index: u16,
    }
}

record! {
    /// The item of addr_entries.bin lists: an index into addr_points.bin.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct AddrEntry {
        pub addr_index: u32,
    }
}

record! {
    /// `street_ways.bin`.
    #[derive(Debug, Clone, Copy)]
    pub struct StreetWay {
        /// Byte offset into street_nodes.bin (u64, file exceeds 4 GB at planet).
        pub node_offset: u64,
        /// String offset into strings.bin.
        pub name_offset: u32,
        pub node_count: u16,
    }
}

record! {
    /// `addr_points.bin`.
    #[derive(Debug, Clone, Copy)]
    pub struct AddrPoint {
        pub lat_e7: i32,
        pub lon_e7: i32,
        pub housenumber_offset: u32,
        pub street_offset: u32,
        pub postcode_offset: u32,
    }
}

record! {
    /// `interp_ways.bin`.
    #[derive(Debug, Clone, Copy)]
    pub struct InterpWay {
        /// Byte offset into interp_nodes.bin (u64 for consistency).
        pub node_offset: u64,
        pub street_offset: u32,
        pub start_number: u32,
        pub end_number: u32,
        pub node_count: u16,
        /// Serialized InterpolationType; checked when opening the index.
        pub interpolation_type: u8,
    }
}

record! {
    /// `admin_cells.bin`, sorted by `cell_id`.
    #[derive(Debug, Clone, Copy)]
    pub struct AdminCell {
        pub cell_id: u64,
        /// Byte offset of this cell's list in admin_entries.bin.
        pub entries_offset: u64,
    }
}

record! {
    /// `admin_polygons.bin`. The trailing reserved byte is for future flags.
    #[derive(Debug, Clone, Copy)]
    pub struct AdminPolygon {
        /// Approximate area in square degrees (for smallest-polygon selection).
        pub area: f32,
        /// Byte offset into admin_vertices.bin.
        /// Builder-produced exterior and hole rings repeat their first vertex at
        /// the end; RING_SENTINEL separates rings. The closing edge is therefore
        /// included in adjacent vertex pairs. Generic ring parsing also accepts
        /// implicit closure, but does not certify this producer invariant.
        pub vertex_offset: u64,
        pub vertex_count: u32,
        pub name_offset: u32,
        /// String offset for ISO 3166-1 alpha2 country code (0 = none).
        /// Only populated for admin_level=2 boundaries. The builder interns
        /// the 2-char code (e.g., "DK") into the string pool.
        pub country_code_offset: u32,
        /// Administrative level 2-10, or POSTAL_BOUNDARY_LEVEL for postal boundaries.
        pub admin_level: u8,
    }
    reserved = 1;
}

record! {
    /// A coordinate pair: the record of street_nodes.bin, interp_nodes.bin
    /// and admin_vertices.bin.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct NodeCoord {
        pub lat_e7: i32,
        pub lon_e7: i32,
    }
}

/// Sentinel value for ring separator in admin_vertices.bin.
pub const RING_SENTINEL: NodeCoord = NodeCoord {
    lat_e7: i32::MIN,
    lon_e7: i32::MIN,
};

/// The item of admin_entries.bin lists: a polygon index with the interior
/// hint packed into its high bit ([`INTERIOR_FLAG`]). Hand-written rather
/// than a `record!` because its two values share one u32.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdminEntry {
    raw: u32,
}

impl AdminEntry {
    /// Bytes per record on disk.
    pub const SIZE: usize = <u32 as WireField>::WIDTH;

    /// `None` when `polygon_index` reaches into the flag bit: masking it
    /// would silently name a different polygon.
    pub fn new(polygon_index: u32, interior_hint: bool) -> Option<Self> {
        if polygon_index & INTERIOR_FLAG != 0 {
            return None;
        }
        let flag = if interior_hint { INTERIOR_FLAG } else { 0 };
        Some(Self {
            raw: polygon_index | flag,
        })
    }

    #[inline]
    pub fn polygon_index(self) -> u32 {
        self.raw & INDEX_MASK
    }

    /// A hint only: the reader still runs the point-in-polygon test.
    #[inline]
    pub fn interior_hint(self) -> bool {
        self.raw & INTERIOR_FLAG != 0
    }

    #[inline]
    pub fn to_bytes(self) -> [u8; Self::SIZE] {
        self.raw.to_le_bytes()
    }

    #[inline]
    pub fn from_bytes(buf: &[u8; Self::SIZE]) -> Self {
        Self {
            raw: u32::from_le_bytes(*buf),
        }
    }
}

impl sealed::Sealed for AdminEntry {}

impl Record for AdminEntry {
    const SIZE: usize = AdminEntry::SIZE;

    #[inline]
    fn read(bytes: &[u8]) -> Option<Self> {
        bytes.try_into().ok().map(Self::from_bytes)
    }
}

// ---------------------------------------------------------------------------
// Record runs: whole files, fixed-count spans and length-prefixed lists
// ---------------------------------------------------------------------------

/// The count prefix of every entry list (the per-cell lists in the street,
/// address, interpolation and admin entry files). A list is this count,
/// little-endian, followed by that many records. Widening it is a format
/// version bump.
pub type ListCount = u16;

/// Bytes of a list's count prefix.
pub const LIST_COUNT_SIZE: usize = std::mem::size_of::<ListCount>();

/// Bytes a list of `count` `T` records occupies, prefix included.
pub fn list_byte_len<T: Record>(count: ListCount) -> u64 {
    // At most u16::MAX * 32 + 2: no overflow is possible.
    (LIST_COUNT_SIZE + usize::from(count) * T::SIZE) as u64
}

/// Why a span or list does not fit the file it points into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunError {
    /// The byte offset does not fit this platform's `usize`.
    OffsetUnaddressable,
    /// A list's count prefix is not inside the file.
    CountOutOfBounds,
    /// The run's end offset overflows.
    SizeOverflow,
    /// The run extends past the end of the file.
    PastEnd,
}

/// A run of `T` records over borrowed bytes: a whole record file, a
/// fixed-count span, or a list's items. Records are read where they lie; no
/// alignment is assumed.
pub struct Table<'a, T> {
    bytes: &'a [u8],
    record: std::marker::PhantomData<fn() -> T>,
}

impl<T> Clone for Table<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Table<'_, T> {}

impl<T> Default for Table<'_, T> {
    fn default() -> Self {
        Self {
            bytes: &[],
            record: std::marker::PhantomData,
        }
    }
}

impl<'a, T: Record> Table<'a, T> {
    const NONZERO_SIZE: () = assert!(T::SIZE > 0);

    /// A whole record file. A trailing partial record is not reachable
    /// through the table; the reader's open-time length check is what
    /// rejects a file that has one.
    #[inline]
    pub fn new(bytes: &'a [u8]) -> Self {
        let () = Self::NONZERO_SIZE;
        Self {
            bytes,
            record: std::marker::PhantomData,
        }
    }

    /// `count` records starting at byte `offset` of `bytes`. A zero count is
    /// an empty run whatever the offset, as the format writes for empty
    /// geometry.
    #[inline]
    pub fn span(bytes: &'a [u8], offset: u64, count: u64) -> Result<Self, RunError> {
        if count == 0 {
            return Ok(Self::default());
        }
        let start = usize::try_from(offset).map_err(|_| RunError::OffsetUnaddressable)?;
        let end = usize::try_from(count)
            .ok()
            .and_then(|n| n.checked_mul(T::SIZE))
            .and_then(|len| start.checked_add(len))
            .ok_or(RunError::SizeOverflow)?;
        bytes
            .get(start..end)
            .map(Self::new)
            .ok_or(RunError::PastEnd)
    }

    /// The items of the list at byte `offset` of `bytes`.
    /// [`NO_DATA_U64`] is an empty list.
    #[inline]
    pub fn list(bytes: &'a [u8], offset: u64) -> Result<Self, RunError> {
        if offset == NO_DATA_U64 {
            return Ok(Self::default());
        }
        let start = usize::try_from(offset).map_err(|_| RunError::OffsetUnaddressable)?;
        let items = start
            .checked_add(LIST_COUNT_SIZE)
            .ok_or(RunError::SizeOverflow)?;
        let prefix = bytes.get(start..items).ok_or(RunError::CountOutOfBounds)?;
        let mut count = [0u8; LIST_COUNT_SIZE];
        count.copy_from_slice(prefix);
        let count = usize::from(ListCount::from_le_bytes(count));
        let end = count
            .checked_mul(T::SIZE)
            .and_then(|len| items.checked_add(len))
            .ok_or(RunError::SizeOverflow)?;
        bytes
            .get(items..end)
            .map(Self::new)
            .ok_or(RunError::PastEnd)
    }

    /// Whole records in the run.
    #[inline]
    pub fn len(&self) -> usize {
        self.bytes.len() / T::SIZE
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Record `index`, or `None` past the end.
    #[inline]
    pub fn get(&self, index: usize) -> Option<T> {
        let start = index.checked_mul(T::SIZE)?;
        let end = start.checked_add(T::SIZE)?;
        T::read(self.bytes.get(start..end)?)
    }

    #[inline]
    pub fn iter(&self) -> impl ExactSizeIterator<Item = T> + 'a {
        self.bytes
            .chunks_exact(T::SIZE)
            .map(|chunk| T::read(chunk).expect("chunks_exact yields exactly Record::SIZE bytes"))
    }

    /// Binary search a run sorted by `key_of`. With duplicate keys, which
    /// one is found is unspecified (the index writes none).
    #[inline]
    pub fn find_by_key<F: Fn(&T) -> u64>(&self, key: u64, key_of: F) -> Option<T> {
        let mut lo = 0usize;
        let mut hi = self.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let record = self.get(mid)?;
            match key_of(&record).cmp(&key) {
                std::cmp::Ordering::Equal => return Some(record),
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Ring parsing
// ---------------------------------------------------------------------------

/// Parse a sequence of [`NodeCoord`] values into rings of `(lon, lat)` f64 pairs,
/// split by [`RING_SENTINEL`]. Rings with fewer than 3 vertices are dropped.
///
/// Returns all rings (exterior first, then holes) as a flat `Vec<Vec<(f64, f64)>>`.
pub fn parse_rings(coords: impl Iterator<Item = NodeCoord>) -> Vec<Vec<(f64, f64)>> {
    let mut rings: Vec<Vec<(f64, f64)>> = Vec::new();
    let mut current: Vec<(f64, f64)> = Vec::new();
    for nc in coords {
        if nc == RING_SENTINEL {
            if current.len() >= 3 {
                rings.push(std::mem::take(&mut current));
            } else {
                current.clear();
            }
        } else {
            current.push((e7_to_deg(nc.lon_e7 as f64), e7_to_deg(nc.lat_e7 as f64)));
        }
    }
    if current.len() >= 3 {
        rings.push(current);
    }
    rings
}

/// Parse rings and split into exterior + hole rings.
///
/// Convenience wrapper around [`parse_rings`] that returns the first ring as the
/// exterior and the rest as holes.
#[allow(clippy::type_complexity)]
pub fn parse_polygon_rings(
    coords: impl Iterator<Item = NodeCoord>,
) -> (Vec<(f64, f64)>, Vec<Vec<(f64, f64)>>) {
    let mut rings = parse_rings(coords);
    if rings.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let exterior = rings.remove(0);
    (exterior, rings)
}

// ---------------------------------------------------------------------------
// Null-terminated string reading
// ---------------------------------------------------------------------------

/// Read a null-terminated UTF-8 string from a byte slice at the given offset.
///
/// Offset 0 is the format's genuine empty string. Invalid offsets, missing NUL
/// terminators, and invalid UTF-8 return `None` so corruption cannot collapse
/// to the same value.
pub fn read_nul_string(data: &[u8], offset: u32) -> Option<&str> {
    if offset == 0 {
        return data.first().is_some_and(|byte| *byte == 0).then_some("");
    }
    let start = offset as usize;
    let remaining = data.get(start..)?;
    let end = remaining.iter().position(|&b| b == 0)?;
    std::str::from_utf8(&remaining[..end]).ok()
}

// ---------------------------------------------------------------------------
// File names
// ---------------------------------------------------------------------------

// Keep individual names: Reader::open binds each file to its typed field,
// rather than iterating an untyped registry. An ALL_FILES list would be a
// third owner requiring updates without proving any binding is correct.
pub const FILE_HEADER: &str = "geocode_header.bin";
pub const FILE_GEO_CELLS: &str = "geo_cells.bin";
pub const FILE_STREET_ENTRIES: &str = "street_entries.bin";
pub const FILE_ADDR_ENTRIES: &str = "addr_entries.bin";
pub const FILE_INTERP_ENTRIES: &str = "interp_entries.bin";
pub const FILE_COARSE_GEO_CELLS: &str = "coarse_geo_cells.bin";
pub const FILE_COARSE_STREET_ENTRIES: &str = "coarse_street_entries.bin";
pub const FILE_COARSE_ADDR_ENTRIES: &str = "coarse_addr_entries.bin";
pub const FILE_COARSE_INTERP_ENTRIES: &str = "coarse_interp_entries.bin";
pub const FILE_STREET_WAYS: &str = "street_ways.bin";
pub const FILE_STREET_NODES: &str = "street_nodes.bin";
pub const FILE_ADDR_POINTS: &str = "addr_points.bin";
pub const FILE_INTERP_WAYS: &str = "interp_ways.bin";
pub const FILE_INTERP_NODES: &str = "interp_nodes.bin";
pub const FILE_ADMIN_CELLS: &str = "admin_cells.bin";
pub const FILE_ADMIN_ENTRIES: &str = "admin_entries.bin";
pub const FILE_ADMIN_POLYGONS: &str = "admin_polygons.bin";
pub const FILE_ADMIN_VERTICES: &str = "admin_vertices.bin";
pub const FILE_STRINGS: &str = "strings.bin";

/// Map a read-only file, including valid empty record files.
#[cfg(feature = "geocode-reader")]
pub(crate) fn mmap_or_empty(path: &std::path::Path) -> crate::Result<memmap2::Mmap> {
    use crate::error::Error;
    let file = crate::read::input::open(path)?;
    let len = file
        .metadata()
        .map_err(|e| Error::from(e).context(format!("failed to stat {}", path.display())))?
        .len();
    let mapped = if len == 0 {
        // memmap2 refuses zero-length files; every consumer bounds-checks
        // records, so an empty anonymous mapping reads as "no records".
        memmap2::MmapOptions::new()
            .map_anon()
            .and_then(memmap2::MmapMut::make_read_only)
            .map_err(|e| {
                Error::from(e).context(format!(
                    "failed to create empty mmap for {}",
                    path.display()
                ))
            })?
    } else {
        // SAFETY: index files must remain unchanged while mapped, in both
        // the builder's post-flush phase and the Reader's lifetime.
        unsafe { memmap2::Mmap::map(&file) }
            .map_err(|e| Error::from(e).context(format!("failed to mmap {}", path.display())))?
    };
    Ok(mapped)
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Format-level errors during header parsing.
#[derive(Debug)]
pub enum FormatError {
    BadMagic,
    UnsupportedVersion(u32),
    InvalidMetadata(String),
}

/// Shared by the builder's config validation and the reader's header parse,
/// so both sides accept exactly the same radii. The caller supplies the error
/// framing: a bad build config is not bad index metadata.
pub(crate) fn validate_radius(name: &str, radius: f32) -> Result<(), RadiusError> {
    if !radius.is_finite() || radius < 0.0 {
        return Err(RadiusError {
            name: name.to_owned(),
            radius,
        });
    }
    Ok(())
}

/// A search radius that is not a finite non-negative number. A neutral leaf:
/// the builder config check classifies it as an invalid argument, the header
/// parse as invalid index metadata.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RadiusError {
    name: String,
    radius: f32,
}

impl std::fmt::Display for RadiusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} must be a finite non-negative number, got {}",
            self.name, self.radius
        )
    }
}

impl std::error::Error for RadiusError {}

impl std::fmt::Display for FormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadMagic => write!(f, "invalid geocode index header (expected GIDX magic)"),
            Self::UnsupportedVersion(v) => {
                write!(
                    f,
                    "unsupported geocode index version {v} (this build reads \
                     version {FORMAT_VERSION}): rebuild the index with \
                     build-geocode-index"
                )
            }
            Self::InvalidMetadata(message) => {
                write!(f, "invalid geocode index metadata: {message}")
            }
        }
    }
}

impl std::error::Error for FormatError {}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_header_bytes() -> [u8; HEADER_SIZE] {
        Header {
            format_version: FORMAT_VERSION,
            street_cell_level: 17,
            coarse_cell_level: 14,
            admin_cell_level: 10,
            max_admin_vertices: 500,
            fine_search_radius_m: 75.0,
            coarse_search_radius_m: 1000.0,
            replication_sequence: 0,
            replication_timestamp: 0,
            addr_point_count: 0,
            street_way_count: 0,
            interp_way_count: 0,
            admin_polygon_count: 0,
            geo_cell_count: 0,
            coarse_cell_count: 0,
            admin_cell_count: 0,
        }
        .to_bytes()
    }

    /// Every level field, not just the street level. A test that pokes one of
    /// three byte positions cannot tell a check on all three from a check on
    /// the one it happens to poke.
    #[test]
    fn header_rejects_out_of_range_s2_levels_in_every_field() {
        assert!(
            Header::from_bytes(&valid_header_bytes()).is_ok(),
            "the baseline header must parse, or every case below is vacuous"
        );
        for (name, byte) in [("street", 8usize), ("coarse", 9), ("admin", 10)] {
            let mut header = valid_header_bytes();
            header[byte] = 31;
            let err = Header::from_bytes(&header)
                .expect_err("an out-of-range S2 level must be rejected while reading");
            assert!(
                err.to_string().contains("S2 cell levels must be in 0..=30"),
                "{name}: unexpected error: {err}"
            );
        }
    }

    /// The ordering rule is a separate rule from the range rule and needs its
    /// own case: a header with coarse >= street is in range on every field,
    /// so the range check above cannot catch it. Without this the ordering
    /// check could be deleted outright and nothing would fail.
    #[test]
    fn header_rejects_coarse_level_not_finer_than_street_level() {
        for coarse in [17u8, 18, 30] {
            let mut header = valid_header_bytes();
            header[9] = coarse;
            let err = Header::from_bytes(&header)
                .expect_err("coarse level at or above street level must be rejected");
            assert!(
                err.to_string()
                    .contains("must be less than street S2 level"),
                "coarse={coarse}: unexpected error: {err}"
            );
        }
    }

    #[test]
    fn header_rejects_invalid_search_radii() {
        assert!(
            Header::from_bytes(&valid_header_bytes()).is_ok(),
            "the baseline header must parse, or every case below is vacuous"
        );
        for byte in [16usize, 20] {
            let mut header = valid_header_bytes();
            header[byte..byte + 4].copy_from_slice(&0.0f32.to_le_bytes());
            assert!(
                Header::from_bytes(&header).is_ok(),
                "zero is a valid non-negative search radius"
            );
        }
        for (name, byte) in [("fine", 16usize), ("coarse", 20)] {
            for radius in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -1.0] {
                let mut header = valid_header_bytes();
                header[byte..byte + 4].copy_from_slice(&radius.to_le_bytes());
                let err = Header::from_bytes(&header)
                    .expect_err("a non-finite or negative search radius must be rejected");
                assert!(
                    err.to_string()
                        .contains("must be a finite non-negative number"),
                    "{name} radius {radius}: unexpected error: {err}"
                );
            }
        }
    }

    #[test]
    fn header_roundtrip() {
        let header = Header {
            format_version: FORMAT_VERSION,
            street_cell_level: 17,
            coarse_cell_level: 14,
            admin_cell_level: 10,
            max_admin_vertices: 500,
            fine_search_radius_m: 75.0,
            coarse_search_radius_m: 1000.0,
            replication_sequence: 4704,
            replication_timestamp: 1_700_000_000,
            addr_point_count: 3_000_000,
            street_way_count: 1_800_000,
            interp_way_count: 100_000,
            admin_polygon_count: 5_000,
            geo_cell_count: 1_500_000,
            coarse_cell_count: 200_000,
            admin_cell_count: 50_000,
        };
        let bytes = header.to_bytes();
        assert_eq!(bytes.len(), HEADER_SIZE);
        let parsed = Header::from_bytes(&bytes).expect("valid header");
        assert_eq!(parsed.format_version, FORMAT_VERSION);
        assert_eq!(parsed.street_cell_level, 17);
        assert_eq!(parsed.coarse_cell_level, 14);
        assert_eq!(parsed.admin_cell_level, 10);
        assert_eq!(parsed.max_admin_vertices, 500);
        assert!((parsed.fine_search_radius_m - 75.0).abs() < f32::EPSILON);
        assert!((parsed.coarse_search_radius_m - 1000.0).abs() < f32::EPSILON);
        assert_eq!(parsed.replication_sequence, 4704);
        assert_eq!(parsed.replication_timestamp, 1_700_000_000);
        assert_eq!(parsed.addr_point_count, 3_000_000);
        assert_eq!(parsed.street_way_count, 1_800_000);
        assert_eq!(parsed.interp_way_count, 100_000);
        assert_eq!(parsed.admin_polygon_count, 5_000);
        assert_eq!(parsed.geo_cell_count, 1_500_000);
        assert_eq!(parsed.coarse_cell_count, 200_000);
        assert_eq!(parsed.admin_cell_count, 50_000);
    }

    #[test]
    fn interpolation_wire_values_are_closed() {
        for value in 0..=u8::MAX {
            assert_eq!(InterpolationType::try_from(value).is_ok(), value <= 2);
        }
        assert_eq!(InterpolationType::All as u8, 0);
        assert_eq!(InterpolationType::Even as u8, 1);
        assert_eq!(InterpolationType::Odd as u8, 2);
    }

    #[test]
    fn header_bad_magic() {
        let mut bytes = [0u8; HEADER_SIZE];
        bytes[0..4].copy_from_slice(b"XXXX");
        assert!(matches!(
            Header::from_bytes(&bytes),
            Err(FormatError::BadMagic)
        ));
    }

    #[test]
    fn header_bad_version() {
        let mut bytes = [0u8; HEADER_SIZE];
        bytes[0..4].copy_from_slice(&HEADER_MAGIC);
        bytes[4..8].copy_from_slice(&99u32.to_le_bytes());
        assert!(matches!(
            Header::from_bytes(&bytes),
            Err(FormatError::UnsupportedVersion(99))
        ));
    }

    #[test]
    fn version_two_requires_rebuild() {
        let mut bytes = valid_header_bytes();
        bytes[4..8].copy_from_slice(&2u32.to_le_bytes());
        let err = Header::from_bytes(&bytes).expect_err("version 2 must be rejected");
        assert!(matches!(err, FormatError::UnsupportedVersion(2)));
        // The message is the entire user-facing surface for this - there is no
        // CLI query command - so it must name the remedy, not just the number.
        let message = err.to_string();
        assert!(
            message.contains("rebuild"),
            "v2 rejection must tell the user to rebuild, got: {message}"
        );
        assert!(
            message.contains("build-geocode-index"),
            "v2 rejection must name the command that rebuilds, got: {message}"
        );
    }

    #[test]
    fn geo_cell_roundtrip() {
        let cell = GeoCell {
            cell_id: 0x1234_5678_9ABC_DEF0,
            street_offset: 0xAAAA_BBBB_CCCC_DDDD,
            addr_offset: 0x1111_2222_3333_4444,
            interp_offset: NO_DATA_U64,
        };
        let bytes = cell.to_bytes();
        let parsed = GeoCell::from_bytes(&bytes);
        assert_eq!(parsed.cell_id, cell.cell_id);
        assert_eq!(parsed.street_offset, cell.street_offset);
        assert_eq!(parsed.addr_offset, cell.addr_offset);
        assert_eq!(parsed.interp_offset, NO_DATA_U64);
    }

    #[test]
    fn segment_ref_roundtrip() {
        let sr = SegmentRef {
            way_index: 42_000,
            segment_index: 17,
        };
        let bytes = sr.to_bytes();
        let parsed = SegmentRef::from_bytes(&bytes);
        assert_eq!(parsed, sr);
    }

    #[test]
    fn street_way_roundtrip() {
        let way = StreetWay {
            node_offset: 24_000_000_000, // >4 GB
            name_offset: 12345,
            node_count: 50,
        };
        let bytes = way.to_bytes();
        let parsed = StreetWay::from_bytes(&bytes);
        assert_eq!(parsed.node_offset, 24_000_000_000);
        assert_eq!(parsed.name_offset, 12345);
        assert_eq!(parsed.node_count, 50);
    }

    #[test]
    fn addr_point_roundtrip() {
        let pt = AddrPoint {
            lat_e7: 556_761_000,
            lon_e7: 125_683_000,
            housenumber_offset: 100,
            street_offset: 200,
            postcode_offset: 0, // no postcode
        };
        let bytes = pt.to_bytes();
        let parsed = AddrPoint::from_bytes(&bytes);
        assert_eq!(parsed.lat_e7, 556_761_000);
        assert_eq!(parsed.lon_e7, 125_683_000);
        assert_eq!(parsed.housenumber_offset, 100);
        assert_eq!(parsed.street_offset, 200);
        assert_eq!(parsed.postcode_offset, 0);
    }

    #[test]
    fn interp_way_roundtrip() {
        let iw = InterpWay {
            node_offset: 50000,
            street_offset: 300,
            start_number: 2,
            end_number: 50,
            node_count: 5,
            interpolation_type: 1, // even
        };
        let bytes = iw.to_bytes();
        let parsed = InterpWay::from_bytes(&bytes);
        assert_eq!(parsed.node_offset, 50000);
        assert_eq!(parsed.street_offset, 300);
        assert_eq!(parsed.start_number, 2);
        assert_eq!(parsed.end_number, 50);
        assert_eq!(parsed.node_count, 5);
        assert_eq!(parsed.interpolation_type, 1);
    }

    #[test]
    fn admin_polygon_roundtrip() {
        let poly = AdminPolygon {
            area: 123.456,
            vertex_offset: u64::from(u32::MAX) + 8000,
            vertex_count: 500,
            name_offset: 400,
            country_code_offset: 42, // string pool offset for "DK"
            admin_level: 2,
        };
        let bytes = poly.to_bytes();
        assert_eq!(bytes.len(), 26);
        assert_eq!(bytes[25], 0, "the reserved byte is written as zero");
        assert_eq!(
            u64::from_le_bytes(bytes[4..12].try_into().expect("8-byte vertex offset")),
            poly.vertex_offset,
            "independent byte decoder must see the full producer offset"
        );
        let parsed = AdminPolygon::from_bytes(&bytes);
        assert!((parsed.area - 123.456).abs() < 0.001);
        assert_eq!(parsed.vertex_offset, u64::from(u32::MAX) + 8000);
        assert_eq!(parsed.vertex_count, 500);
        assert_eq!(parsed.name_offset, 400);
        assert_eq!(parsed.country_code_offset, 42);
        assert_eq!(parsed.admin_level, 2);
    }

    #[test]
    fn admin_cell_roundtrip_preserves_offset_past_four_gibibytes() {
        let cell = AdminCell {
            cell_id: 0x1234_5678_9abc_def0,
            entries_offset: u64::from(u32::MAX) + 123,
        };
        let bytes = cell.to_bytes();
        assert_eq!(
            u64::from_le_bytes(bytes[8..16].try_into().expect("8-byte entries offset")),
            cell.entries_offset,
            "independent byte decoder must see the full producer offset"
        );
        let parsed = AdminCell::from_bytes(&bytes);
        assert_eq!(parsed.cell_id, cell.cell_id);
        assert_eq!(parsed.entries_offset, cell.entries_offset);
    }

    #[test]
    fn nul_string_distinguishes_empty_from_corruption() {
        let pool = b"\0valid\0bad\xff\0unterminated";
        assert_eq!(read_nul_string(pool, 0), Some(""));
        assert_eq!(read_nul_string(pool, 1), Some("valid"));
        assert_eq!(read_nul_string(pool, 7), None, "invalid UTF-8");
        assert_eq!(read_nul_string(pool, 12), None, "missing NUL");
        assert_eq!(read_nul_string(pool, 99), None, "out-of-bounds offset");
        assert_eq!(
            read_nul_string(b"not-empty", 0),
            None,
            "offset zero needs NUL"
        );
    }

    #[test]
    fn node_coord_roundtrip() {
        let nc = NodeCoord {
            lat_e7: -556_761_000,
            lon_e7: 125_683_000,
        };
        let bytes = nc.to_bytes();
        let parsed = NodeCoord::from_bytes(&bytes);
        assert_eq!(parsed, nc);
    }

    #[test]
    fn ring_sentinel() {
        let bytes = RING_SENTINEL.to_bytes();
        let parsed = NodeCoord::from_bytes(&bytes);
        assert_eq!(parsed, RING_SENTINEL);
    }

    // -----------------------------------------------------------------------
    // parse_polygon_rings tests
    // -----------------------------------------------------------------------

    fn nc(lat: i32, lon: i32) -> NodeCoord {
        NodeCoord {
            lat_e7: lat,
            lon_e7: lon,
        }
    }

    #[test]
    fn parse_polygon_rings_single_ring() {
        // Three vertices, no sentinel → exterior only, no holes
        let coords = vec![nc(10, 20), nc(30, 40), nc(50, 60)];
        let (exterior, holes) = parse_polygon_rings(coords.into_iter());
        assert_eq!(exterior.len(), 3);
        assert!(holes.is_empty());
    }

    #[test]
    fn parse_polygon_rings_exterior_plus_hole() {
        // Exterior ring, then RING_SENTINEL, then hole ring
        let coords = vec![
            nc(10, 20),
            nc(30, 40),
            nc(50, 60),
            RING_SENTINEL,
            nc(15, 25),
            nc(35, 45),
            nc(55, 65),
        ];
        let (exterior, holes) = parse_polygon_rings(coords.into_iter());
        assert_eq!(exterior.len(), 3);
        assert_eq!(holes.len(), 1);
        assert_eq!(holes[0].len(), 3);
    }

    #[test]
    fn parse_polygon_rings_short_ring_dropped() {
        // A ring with fewer than 3 vertices gets dropped
        let coords = vec![
            nc(10, 20),
            nc(30, 40), // 2 vertices - too short
        ];
        let (exterior, holes) = parse_polygon_rings(coords.into_iter());
        assert!(exterior.is_empty());
        assert!(holes.is_empty());
    }

    #[test]
    fn parse_polygon_rings_empty_input() {
        let coords: Vec<NodeCoord> = vec![];
        let (exterior, holes) = parse_polygon_rings(coords.into_iter());
        assert!(exterior.is_empty());
        assert!(holes.is_empty());
    }

    #[test]
    fn parse_polygon_rings_consecutive_sentinels_no_empty_rings() {
        // Multiple sentinels in a row should not produce empty rings
        let coords = vec![
            nc(10, 20),
            nc(30, 40),
            nc(50, 60),
            RING_SENTINEL,
            RING_SENTINEL,
            RING_SENTINEL,
            nc(15, 25),
            nc(35, 45),
            nc(55, 65),
        ];
        let (exterior, holes) = parse_polygon_rings(coords.into_iter());
        assert_eq!(exterior.len(), 3);
        assert_eq!(
            holes.len(),
            1,
            "consecutive sentinels should not create empty rings"
        );
        assert_eq!(holes[0].len(), 3);
    }

    #[test]
    fn ring_sentinel_values() {
        // Verify RING_SENTINEL uses i32::MIN, not (0, 0)
        assert_eq!(RING_SENTINEL.lat_e7, i32::MIN);
        assert_eq!(RING_SENTINEL.lon_e7, i32::MIN);
    }

    // -----------------------------------------------------------------------
    // Wire layout, checked independently of the record codec
    // -----------------------------------------------------------------------

    /// The format's record sizes, written out rather than derived: `SIZE` is
    /// computed from each field list, so a field added or retyped by mistake
    /// changes it silently unless something states the expected value.
    #[test]
    fn record_sizes_are_the_format_sizes() {
        assert_eq!(GeoCell::SIZE, 32);
        assert_eq!(SegmentRef::SIZE, 6);
        assert_eq!(AddrEntry::SIZE, 4);
        assert_eq!(StreetWay::SIZE, 14);
        assert_eq!(AddrPoint::SIZE, 20);
        assert_eq!(InterpWay::SIZE, 23);
        assert_eq!(AdminCell::SIZE, 16);
        assert_eq!(AdminPolygon::SIZE, 26);
        assert_eq!(NodeCoord::SIZE, 8);
        assert_eq!(AdminEntry::SIZE, 4);
        assert_eq!(LIST_COUNT_SIZE, 2);
    }

    /// Byte positions written out by hand. A round trip through the same
    /// codec cannot catch a layout error both directions share.
    #[test]
    fn records_encode_fields_at_their_format_offsets() {
        let mut expected = Vec::new();
        expected.extend_from_slice(&0x0102_0304_0506_0708u64.to_le_bytes());
        expected.extend_from_slice(&0x1112_1314u32.to_le_bytes());
        expected.extend_from_slice(&0x2122u16.to_le_bytes());
        let way = StreetWay {
            node_offset: 0x0102_0304_0506_0708,
            name_offset: 0x1112_1314,
            node_count: 0x2122,
        };
        assert_eq!(way.to_bytes().as_slice(), expected.as_slice());

        let iw = InterpWay {
            node_offset: 7,
            street_offset: 8,
            start_number: 9,
            end_number: 10,
            node_count: 11,
            interpolation_type: 2,
        };
        let bytes = iw.to_bytes();
        assert_eq!(&bytes[0..8], &7u64.to_le_bytes());
        assert_eq!(&bytes[8..12], &8u32.to_le_bytes());
        assert_eq!(&bytes[12..16], &9u32.to_le_bytes());
        assert_eq!(&bytes[16..20], &10u32.to_le_bytes());
        assert_eq!(&bytes[20..22], &11u16.to_le_bytes());
        assert_eq!(bytes[22], 2);

        let pt = AddrPoint {
            lat_e7: -1,
            lon_e7: 2,
            housenumber_offset: 3,
            street_offset: 4,
            postcode_offset: 5,
        };
        let bytes = pt.to_bytes();
        assert_eq!(&bytes[0..4], &(-1i32).to_le_bytes());
        assert_eq!(&bytes[4..8], &2i32.to_le_bytes());
        assert_eq!(&bytes[16..20], &5u32.to_le_bytes());

        let sr = SegmentRef {
            way_index: 0xAABB_CCDD,
            segment_index: 0xEEFF,
        };
        assert_eq!(sr.to_bytes(), [0xDD, 0xCC, 0xBB, 0xAA, 0xFF, 0xEE]);

        let gc = GeoCell {
            cell_id: 1,
            street_offset: 2,
            addr_offset: 3,
            interp_offset: 4,
        };
        let bytes = gc.to_bytes();
        assert_eq!(&bytes[16..24], &3u64.to_le_bytes(), "addr_offset");
        assert_eq!(&bytes[24..32], &4u64.to_le_bytes(), "interp_offset");

        let nc = NodeCoord {
            lat_e7: 0x0102_0304,
            lon_e7: -2,
        };
        assert_eq!(&nc.to_bytes()[0..4], &0x0102_0304i32.to_le_bytes());
        assert_eq!(
            AddrEntry {
                addr_index: 0x0A0B_0C0D
            }
            .to_bytes(),
            [0x0D, 0x0C, 0x0B, 0x0A]
        );
    }

    #[test]
    fn admin_polygon_ignores_its_reserved_byte_on_read() {
        let poly = AdminPolygon {
            area: f32::from_bits(0x7FC0_0001),
            vertex_offset: 1,
            vertex_count: 2,
            name_offset: 3,
            country_code_offset: 4,
            admin_level: 5,
        };
        let mut bytes = poly.to_bytes();
        bytes[25] = 0xFF;
        let parsed = AdminPolygon::from_bytes(&bytes);
        assert_eq!(parsed.admin_level, 5);
        assert_eq!(
            parsed.area.to_bits(),
            0x7FC0_0001,
            "float bits survive, NaN payload included"
        );
        assert_eq!(parsed.to_bytes()[25], 0);
    }

    #[test]
    fn admin_entry_packs_the_interior_hint_into_the_high_bit() {
        let plain = AdminEntry::new(7, false).expect("small index");
        let interior = AdminEntry::new(7, true).expect("small index");
        assert_eq!(plain.to_bytes(), 7u32.to_le_bytes());
        assert_eq!(interior.to_bytes(), (7u32 | 0x8000_0000).to_le_bytes());
        let parsed = AdminEntry::from_bytes(&interior.to_bytes());
        assert_eq!(parsed.polygon_index(), 7);
        assert!(parsed.interior_hint());
        assert!(!plain.interior_hint());
        assert_eq!(
            AdminEntry::new(INDEX_MASK, false).map(AdminEntry::polygon_index),
            Some(INDEX_MASK)
        );
        assert!(
            AdminEntry::new(INDEX_MASK + 1, false).is_none(),
            "an index reaching the flag bit must be refused, not masked"
        );
    }

    // -----------------------------------------------------------------------
    // Table, span and list framing
    // -----------------------------------------------------------------------

    fn coords(n: i32) -> Vec<u8> {
        (0..n).flat_map(|i| nc(i, -i).to_bytes()).collect()
    }

    #[test]
    fn table_reads_whole_records_only() {
        let mut bytes = coords(3);
        bytes.push(0xAA);
        let table = Table::<NodeCoord>::new(&bytes);
        assert_eq!(table.len(), 3);
        assert_eq!(table.get(2), Some(nc(2, -2)));
        assert_eq!(table.get(3), None, "the trailing partial record");
        assert_eq!(table.get(usize::MAX), None);
        assert_eq!(table.iter().count(), 3);
        assert!(Table::<NodeCoord>::new(&[]).get(0).is_none());
    }

    #[test]
    fn span_is_byte_addressed_and_bounded() {
        let mut bytes = vec![0u8; 3];
        bytes.extend_from_slice(&coords(4));
        // An unaligned byte offset is legal: spans are not record-indexed.
        let span = Table::<NodeCoord>::span(&bytes, 3 + 8, 2).expect("in bounds");
        assert_eq!(span.iter().collect::<Vec<_>>(), vec![nc(1, -1), nc(2, -2)]);
        assert_eq!(span.get(2), None, "a span ends at its count, not the file");
        assert_eq!(
            Table::<NodeCoord>::span(&bytes, 3 + 8, 4).err(),
            Some(RunError::PastEnd)
        );
        assert_eq!(
            Table::<NodeCoord>::span(&bytes, u64::MAX - 1, 1).err(),
            Some(if usize::BITS >= 64 {
                RunError::SizeOverflow
            } else {
                RunError::OffsetUnaddressable
            })
        );
        assert_eq!(
            Table::<NodeCoord>::span(&bytes, 0, u64::MAX).err(),
            Some(RunError::SizeOverflow)
        );
        for offset in [0, 999, u64::MAX] {
            let empty = Table::<NodeCoord>::span(&bytes, offset, 0)
                .expect("a zero count is empty wherever it points");
            assert!(empty.is_empty());
        }
    }

    #[test]
    fn list_reads_its_count_prefix() {
        let mut bytes = vec![0xEEu8];
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&AddrEntry { addr_index: 5 }.to_bytes());
        bytes.extend_from_slice(&AddrEntry { addr_index: 6 }.to_bytes());
        let list = Table::<AddrEntry>::list(&bytes, 1).expect("well-formed list");
        assert_eq!(
            list.iter().map(|e| e.addr_index).collect::<Vec<_>>(),
            vec![5, 6]
        );
        assert_eq!(list_byte_len::<AddrEntry>(2), 10);
        assert!(
            Table::<AddrEntry>::list(&bytes, NO_DATA_U64)
                .expect("the no-data sentinel")
                .is_empty()
        );
        assert_eq!(
            Table::<AddrEntry>::list(&bytes[..bytes.len() - 1], 1).err(),
            Some(RunError::PastEnd),
            "a count that overruns the file rejects the whole list"
        );
        assert_eq!(
            Table::<AddrEntry>::list(&bytes, 10).err(),
            Some(RunError::CountOutOfBounds)
        );
        assert_eq!(
            Table::<AddrEntry>::list(&bytes, 0).err(),
            Some(RunError::PastEnd),
            "0xEE 0x02 reads as a count far past the end"
        );
    }

    #[test]
    fn find_by_key_finds_present_keys_only() {
        let cells: Vec<u8> = [10u64, 20, 30]
            .iter()
            .flat_map(|&cell_id| {
                AdminCell {
                    cell_id,
                    entries_offset: cell_id + 1,
                }
                .to_bytes()
            })
            .collect();
        let table = Table::<AdminCell>::new(&cells);
        for key in [10, 20, 30] {
            let found = table.find_by_key(key, |c| c.cell_id).expect("present key");
            assert_eq!(found.entries_offset, key + 1);
        }
        for key in [0, 15, 31, u64::MAX] {
            assert!(table.find_by_key(key, |c| c.cell_id).is_none());
        }
        assert!(
            Table::<AdminCell>::default()
                .find_by_key(10, |c| c.cell_id)
                .is_none()
        );
    }
}
