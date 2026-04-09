//! On-disk format definitions for the reverse geocoding index.
//!
//! All records use little-endian byte order and manual serialization (no
//! `#[repr(C)]` transmutation) to avoid alignment padding issues.
//!
//! See `notes/reverse-geocoding-spec.md` section 4 for the full format specification.

/// Magic bytes for the index header: `GIDX`.
pub const HEADER_MAGIC: [u8; 4] = *b"GIDX";

/// Current format version. Version 2: GeoCell widened from 24 to 32 bytes
/// (addr_offset and interp_offset changed from u32 to u64).
pub const FORMAT_VERSION: u32 = 2;

/// Sentinel value for "no data" in u32 offset fields.
/// Sentinel value for "no data" in u64 offset fields.
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
        Ok(Self {
            format_version: version,
            street_cell_level: buf[8],
            coarse_cell_level: buf[9],
            admin_cell_level: buf[10],
            max_admin_vertices: u16::from_le_bytes([buf[12], buf[13]]),
            fine_search_radius_m: f32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]),
            coarse_search_radius_m: f32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]),
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
// geo_cells.bin (24 bytes per record)
// ---------------------------------------------------------------------------

/// Merged street-level cell index entry.
pub const GEO_CELL_SIZE: usize = 32;

#[derive(Debug, Clone, Copy)]
pub struct GeoCell {
    pub cell_id: u64,
    /// Byte offset into street_entries.bin.
    pub street_offset: u64,
    /// Byte offset into addr_entries.bin.
    pub addr_offset: u64,
    /// Byte offset into interp_entries.bin.
    pub interp_offset: u64,
}

impl GeoCell {
    pub fn to_bytes(&self) -> [u8; GEO_CELL_SIZE] {
        let mut buf = [0u8; GEO_CELL_SIZE];
        buf[0..8].copy_from_slice(&self.cell_id.to_le_bytes());
        buf[8..16].copy_from_slice(&self.street_offset.to_le_bytes());
        buf[16..24].copy_from_slice(&self.addr_offset.to_le_bytes());
        buf[24..32].copy_from_slice(&self.interp_offset.to_le_bytes());
        buf
    }

    pub fn from_bytes(buf: &[u8; GEO_CELL_SIZE]) -> Self {
        Self {
            cell_id: u64::from_le_bytes([buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7]]),
            street_offset: u64::from_le_bytes([buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15]]),
            addr_offset: u64::from_le_bytes([buf[16], buf[17], buf[18], buf[19], buf[20], buf[21], buf[22], buf[23]]),
            interp_offset: u64::from_le_bytes([buf[24], buf[25], buf[26], buf[27], buf[28], buf[29], buf[30], buf[31]]),
        }
    }
}

// ---------------------------------------------------------------------------
// Segment ref (6 bytes, used in street_entries and interp_entries)
// ---------------------------------------------------------------------------

/// A reference to a specific segment within a way.
pub const SEGMENT_REF_SIZE: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentRef {
    pub way_index: u32,
    pub segment_index: u16,
}

impl SegmentRef {
    pub fn to_bytes(&self) -> [u8; SEGMENT_REF_SIZE] {
        let mut buf = [0u8; SEGMENT_REF_SIZE];
        buf[0..4].copy_from_slice(&self.way_index.to_le_bytes());
        buf[4..6].copy_from_slice(&self.segment_index.to_le_bytes());
        buf
    }

    pub fn from_bytes(buf: &[u8; SEGMENT_REF_SIZE]) -> Self {
        Self {
            way_index: u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]),
            segment_index: u16::from_le_bytes([buf[4], buf[5]]),
        }
    }
}

// ---------------------------------------------------------------------------
// street_ways.bin (14 bytes per record)
// ---------------------------------------------------------------------------

pub const STREET_WAY_SIZE: usize = 14;

#[derive(Debug, Clone, Copy)]
pub struct StreetWay {
    /// Byte offset into street_nodes.bin (u64, file exceeds 4 GB at planet).
    pub node_offset: u64,
    /// String offset into strings.bin.
    pub name_offset: u32,
    pub node_count: u16,
}

impl StreetWay {
    pub fn to_bytes(&self) -> [u8; STREET_WAY_SIZE] {
        let mut buf = [0u8; STREET_WAY_SIZE];
        buf[0..8].copy_from_slice(&self.node_offset.to_le_bytes());
        buf[8..12].copy_from_slice(&self.name_offset.to_le_bytes());
        buf[12..14].copy_from_slice(&self.node_count.to_le_bytes());
        buf
    }

    pub fn from_bytes(buf: &[u8; STREET_WAY_SIZE]) -> Self {
        Self {
            node_offset: u64::from_le_bytes([buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7]]),
            name_offset: u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]),
            node_count: u16::from_le_bytes([buf[12], buf[13]]),
        }
    }
}

// ---------------------------------------------------------------------------
// addr_points.bin (20 bytes per record)
// ---------------------------------------------------------------------------

pub const ADDR_POINT_SIZE: usize = 20;

#[derive(Debug, Clone, Copy)]
pub struct AddrPoint {
    pub lat_e7: i32,
    pub lon_e7: i32,
    pub housenumber_offset: u32,
    pub street_offset: u32,
    pub postcode_offset: u32,
}

impl AddrPoint {
    pub fn to_bytes(&self) -> [u8; ADDR_POINT_SIZE] {
        let mut buf = [0u8; ADDR_POINT_SIZE];
        buf[0..4].copy_from_slice(&self.lat_e7.to_le_bytes());
        buf[4..8].copy_from_slice(&self.lon_e7.to_le_bytes());
        buf[8..12].copy_from_slice(&self.housenumber_offset.to_le_bytes());
        buf[12..16].copy_from_slice(&self.street_offset.to_le_bytes());
        buf[16..20].copy_from_slice(&self.postcode_offset.to_le_bytes());
        buf
    }

    pub fn from_bytes(buf: &[u8; ADDR_POINT_SIZE]) -> Self {
        Self {
            lat_e7: i32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]),
            lon_e7: i32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]),
            housenumber_offset: u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]),
            street_offset: u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]),
            postcode_offset: u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]),
        }
    }
}

// ---------------------------------------------------------------------------
// interp_ways.bin (23 bytes per record)
// ---------------------------------------------------------------------------

pub const INTERP_WAY_SIZE: usize = 23;

#[derive(Debug, Clone, Copy)]
pub struct InterpWay {
    /// Byte offset into interp_nodes.bin (u64 for consistency).
    pub node_offset: u64,
    pub street_offset: u32,
    pub start_number: u32,
    pub end_number: u32,
    pub node_count: u16,
    /// 0=all, 1=even, 2=odd
    pub interpolation_type: u8,
}

impl InterpWay {
    pub fn to_bytes(&self) -> [u8; INTERP_WAY_SIZE] {
        let mut buf = [0u8; INTERP_WAY_SIZE];
        buf[0..8].copy_from_slice(&self.node_offset.to_le_bytes());
        buf[8..12].copy_from_slice(&self.street_offset.to_le_bytes());
        buf[12..16].copy_from_slice(&self.start_number.to_le_bytes());
        buf[16..20].copy_from_slice(&self.end_number.to_le_bytes());
        buf[20..22].copy_from_slice(&self.node_count.to_le_bytes());
        buf[22] = self.interpolation_type;
        buf
    }

    pub fn from_bytes(buf: &[u8; INTERP_WAY_SIZE]) -> Self {
        Self {
            node_offset: u64::from_le_bytes([buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7]]),
            street_offset: u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]),
            start_number: u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]),
            end_number: u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]),
            node_count: u16::from_le_bytes([buf[20], buf[21]]),
            interpolation_type: buf[22],
        }
    }
}

// ---------------------------------------------------------------------------
// admin_cells.bin (12 bytes per record)
// ---------------------------------------------------------------------------

pub const ADMIN_CELL_SIZE: usize = 12;

#[derive(Debug, Clone, Copy)]
pub struct AdminCell {
    pub cell_id: u64,
    pub entries_offset: u32,
}

impl AdminCell {
    pub fn to_bytes(&self) -> [u8; ADMIN_CELL_SIZE] {
        let mut buf = [0u8; ADMIN_CELL_SIZE];
        buf[0..8].copy_from_slice(&self.cell_id.to_le_bytes());
        buf[8..12].copy_from_slice(&self.entries_offset.to_le_bytes());
        buf
    }

    pub fn from_bytes(buf: &[u8; ADMIN_CELL_SIZE]) -> Self {
        Self {
            cell_id: u64::from_le_bytes([buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7]]),
            entries_offset: u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]),
        }
    }
}

// ---------------------------------------------------------------------------
// admin_polygons.bin (22 bytes per record)
// ---------------------------------------------------------------------------

pub const ADMIN_POLYGON_SIZE: usize = 22;

#[derive(Debug, Clone, Copy)]
pub struct AdminPolygon {
    /// Approximate area in square degrees (for smallest-polygon selection).
    pub area: f32,
    /// Byte offset into admin_vertices.bin.
    pub vertex_offset: u32,
    pub vertex_count: u32,
    pub name_offset: u32,
    /// String offset for ISO 3166-1 alpha2 country code (0 = none).
    /// Only populated for admin_level=2 boundaries. The builder interns
    /// the 2-char code (e.g., "DK") into the string pool.
    pub country_code_offset: u32,
    /// Admin level 2–11.
    pub admin_level: u8,
}

impl AdminPolygon {
    pub fn to_bytes(&self) -> [u8; ADMIN_POLYGON_SIZE] {
        let mut buf = [0u8; ADMIN_POLYGON_SIZE];
        buf[0..4].copy_from_slice(&self.area.to_le_bytes());
        buf[4..8].copy_from_slice(&self.vertex_offset.to_le_bytes());
        buf[8..12].copy_from_slice(&self.vertex_count.to_le_bytes());
        buf[12..16].copy_from_slice(&self.name_offset.to_le_bytes());
        buf[16..20].copy_from_slice(&self.country_code_offset.to_le_bytes());
        buf[20] = self.admin_level;
        // buf[21] reserved
        buf
    }

    pub fn from_bytes(buf: &[u8; ADMIN_POLYGON_SIZE]) -> Self {
        Self {
            area: f32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]),
            vertex_offset: u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]),
            vertex_count: u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]),
            name_offset: u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]),
            country_code_offset: u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]),
            admin_level: buf[20],
        }
    }
}

// ---------------------------------------------------------------------------
// Node coordinate pair (8 bytes, used in street_nodes, interp_nodes, admin_vertices)
// ---------------------------------------------------------------------------

pub const NODE_COORD_SIZE: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeCoord {
    pub lat_e7: i32,
    pub lon_e7: i32,
}

/// Sentinel value for ring separator in admin_vertices.bin.
pub const RING_SENTINEL: NodeCoord = NodeCoord {
    lat_e7: i32::MIN,
    lon_e7: i32::MIN,
};

impl NodeCoord {
    pub fn to_bytes(&self) -> [u8; NODE_COORD_SIZE] {
        let mut buf = [0u8; NODE_COORD_SIZE];
        buf[0..4].copy_from_slice(&self.lat_e7.to_le_bytes());
        buf[4..8].copy_from_slice(&self.lon_e7.to_le_bytes());
        buf
    }

    pub fn from_bytes(buf: &[u8; NODE_COORD_SIZE]) -> Self {
        Self {
            lat_e7: i32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]),
            lon_e7: i32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]),
        }
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
            current.push((nc.lon_e7 as f64 * 1e-7, nc.lat_e7 as f64 * 1e-7));
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
pub fn parse_polygon_rings(coords: impl Iterator<Item = NodeCoord>) -> (Vec<(f64, f64)>, Vec<Vec<(f64, f64)>>) {
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
/// Returns an empty string for offset 0 or out-of-bounds offsets. If the bytes
/// are not valid UTF-8, returns an empty string.
pub fn read_nul_string(data: &[u8], offset: u32) -> &str {
    if offset == 0 {
        return "";
    }
    let start = offset as usize;
    if start >= data.len() {
        return "";
    }
    let remaining = &data[start..];
    let end = remaining.iter().position(|&b| b == 0).unwrap_or(remaining.len());
    std::str::from_utf8(&remaining[..end]).unwrap_or("")
}

// ---------------------------------------------------------------------------
// File names
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Format-level errors during header parsing.
#[derive(Debug)]
pub enum FormatError {
    BadMagic,
    UnsupportedVersion(u32),
}

impl std::fmt::Display for FormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadMagic => write!(f, "invalid geocode index header (expected GIDX magic)"),
            Self::UnsupportedVersion(v) => {
                write!(f, "unsupported geocode index version {v} (expected {FORMAT_VERSION})")
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
    fn header_bad_magic() {
        let mut bytes = [0u8; HEADER_SIZE];
        bytes[0..4].copy_from_slice(b"XXXX");
        assert!(matches!(Header::from_bytes(&bytes), Err(FormatError::BadMagic)));
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
            vertex_offset: 8000,
            vertex_count: 500,
            name_offset: 400,
            country_code_offset: 42, // string pool offset for "DK"
            admin_level: 2,
        };
        let bytes = poly.to_bytes();
        assert_eq!(bytes.len(), ADMIN_POLYGON_SIZE);
        let parsed = AdminPolygon::from_bytes(&bytes);
        assert!((parsed.area - 123.456).abs() < 0.001);
        assert_eq!(parsed.vertex_offset, 8000);
        assert_eq!(parsed.vertex_count, 500);
        assert_eq!(parsed.name_offset, 400);
        assert_eq!(parsed.country_code_offset, 42);
        assert_eq!(parsed.admin_level, 2);
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
}
