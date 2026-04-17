//! Pass 2: fused nodes + ways scan.
//!
//! Sorted PBFs (Sort.Type_then_ID) guarantee all node blobs come before
//! way blobs. A single sequential scan processes nodes first (populating
//! the dense coordinate index for referenced nodes + address points), then
//! ways (streets, buildings, interpolation, admin geometry).

use std::io::{BufWriter, Write};

use rustc_hash::FxHashMap;

use crate::Element;

use super::Result;
use super::BuildConfig;
use super::strings::StringPool;

use super::super::format::*;

// ---------------------------------------------------------------------------
// Highway exclusion list
// ---------------------------------------------------------------------------

pub(super) const EXCLUDED_HIGHWAYS: &[&str] = &[
    "footway", "path", "track", "steps", "cycleway",
    "service", "pedestrian", "bridleway", "construction",
];

// ---------------------------------------------------------------------------
// Intermediate data
// ---------------------------------------------------------------------------

/// Slim interpolation metadata kept in memory during the build.
/// Node coordinates are written directly to interp_nodes.bin;
/// this struct stores only the file offset and count.
///
/// KNOWN LIMITATION (sentinel ambiguity): `start_number == 0 && end_number == 0`
/// is reused as the "unresolved" sentinel -
/// `resolve_interpolation_endpoints_mmap` only overwrites these fields when it
/// finds both endpoints among addr points. A real OSM interpolation way that
/// genuinely starts at house number 0 is indistinguishable from an unresolved
/// one at read time. "0" as a house number is rare but exists in some regions.
/// If the caller ever needs to disambiguate, add a separate `resolved: bool`
/// field here and persist it into [`crate::geocode_index::format::InterpWay`]
/// (requires bumping `FORMAT_VERSION`).
pub(super) struct SlimInterpWay {
    pub(super) street_offset: u32,
    pub(super) interpolation_type: u8,
    pub(super) node_file_offset: u64,
    pub(super) node_count: u16,
    pub(super) start_number: u32,
    pub(super) end_number: u32,
}

// ---------------------------------------------------------------------------
// Pass 2 helper state
// ---------------------------------------------------------------------------

/// Mutable state shared by node and way processing in pass 2.
struct Pass2State<'a> {
    coord_mmap: &'a mut memmap2::MmapMut,
    referenced_nodes: &'a crate::commands::id_set_dense::IdSetDense,
    needed_admin_ways: &'a crate::commands::id_set_dense::IdSetDense,
    way_geom: &'a mut FxHashMap<i64, Vec<(i32, i32)>>,
    strings: &'a mut StringPool,
    interp_ways: &'a mut Vec<SlimInterpWay>,
    // Output writers
    street_ways_out: &'a mut BufWriter<std::fs::File>,
    street_nodes_out: &'a mut BufWriter<std::fs::File>,
    addr_points_out: &'a mut BufWriter<std::fs::File>,
    interp_nodes_out: &'a mut BufWriter<std::fs::File>,
    // Running counters
    street_node_offset: u64,
    interp_node_offset: u64,
    addr_point_count: u32,
    street_way_count: u32,
    first_addr_lat_e7: i32,
    first_addr_lon_e7: i32,
}

impl Pass2State<'_> {
    /// Process a dense node: store coordinates for referenced nodes and
    /// write address points for nodes with addr:housenumber + addr:street.
    fn process_dense_node(&mut self, node: &crate::dense::DenseNode<'_>) -> Result<()> {
        let lat_e7 = node.decimicro_lat();
        let lon_e7 = node.decimicro_lon();
        if self.referenced_nodes.get(node.id()) {
            #[allow(clippy::cast_possible_truncation)]
            let r = self.referenced_nodes.rank(node.id()) as usize;
            let off = r * 8;
            self.coord_mmap[off..off + 4].copy_from_slice(&lat_e7.to_le_bytes());
            self.coord_mmap[off + 4..off + 8].copy_from_slice(&lon_e7.to_le_bytes());
        }

        let mut hn: Option<&str> = None;
        let mut st: Option<&str> = None;
        let mut pc: Option<&str> = None;
        for (k, v) in node.tags() {
            match k {
                "addr:housenumber" => hn = Some(v),
                "addr:street" => st = Some(v),
                "addr:postcode" => pc = Some(v),
                _ => {}
            }
        }
        if let (Some(h), Some(s)) = (hn, st) {
            let ap = AddrPoint {
                lat_e7, lon_e7,
                housenumber_offset: self.strings.intern(h),
                street_offset: self.strings.intern(s),
                postcode_offset: pc.map_or(0, |p| self.strings.intern(p)),
            };
            self.addr_points_out.write_all(&ap.to_bytes())?;
            if self.addr_point_count == 0 {
                self.first_addr_lat_e7 = lat_e7;
                self.first_addr_lon_e7 = lon_e7;
            }
            self.addr_point_count += 1;
        }
        Ok(())
    }

    /// Process a way: classify by tags, resolve coordinates, and write to
    /// the appropriate output (admin geometry, interpolation, building
    /// address, or street).
    #[allow(clippy::too_many_lines)]
    fn process_way(&mut self, way: &crate::elements::Way<'_>) -> Result<()> {
        let way_id = way.id();
        let is_admin_way = self.needed_admin_ways.get(way_id);

        // Tag-first classification: check tags before resolving
        // coordinates. Skips node-index lookups for irrelevant ways.
        let mut highway: Option<&str> = None;
        let mut name: Option<&str> = None;
        let mut hn: Option<&str> = None;
        let mut addr_st: Option<&str> = None;
        let mut pc: Option<&str> = None;
        let mut building = false;
        let mut interp: Option<&str> = None;

        for (k, v) in way.tags() {
            match k {
                "highway" => highway = Some(v),
                "name" => name = Some(v),
                "addr:housenumber" => hn = Some(v),
                "addr:street" => addr_st = Some(v),
                "addr:postcode" => pc = Some(v),
                "building" => building = true,
                "addr:interpolation" => interp = Some(v),
                _ => {}
            }
        }

        let is_street = highway.is_some() && name.is_some()
            && !EXCLUDED_HIGHWAYS.contains(&highway.unwrap_or(""));
        let is_building_addr = building && hn.is_some() && addr_st.is_some();
        let is_interp = interp.is_some() && addr_st.is_some();

        if !is_admin_way && !is_street && !is_building_addr && !is_interp {
            return Ok(());
        }

        let coords: Vec<(i32, i32)> = way.refs()
            .filter_map(|nid| {
                if !self.referenced_nodes.get(nid) { return None; }
                #[allow(clippy::cast_possible_truncation)]
                let r = self.referenced_nodes.rank(nid) as usize;
                let off = r * 8;
                let lat = i32::from_le_bytes(self.coord_mmap[off..off+4].try_into().ok()?);
                let lon = i32::from_le_bytes(self.coord_mmap[off+4..off+8].try_into().ok()?);
                // KNOWN LIMITATION: (0, 0) doubles as the "unresolved" sentinel
                // for the coord mmap (zero-filled on creation, see Pass 2 write
                // path) AND as a legitimate OSM node at Null Island off the
                // African coast. A real node at 0°, 0° - periodically created
                // by broken edits, test fixtures, and GPS-zero errors - is
                // silently dropped here. A proper fix would be a presence
                // bitmap alongside the coord array (one bit per node); not
                // worth the extra pass unless actually observed in production.
                // Same convention is used in ALTW stage 2 (see
                // `src/commands/altw/stage2.rs` `is_resolved`) - fix both
                // together if we ever change the sentinel contract.
                if lat == 0 && lon == 0 { None } else { Some((lat, lon)) }
            })
            .collect();
        if coords.is_empty() { return Ok(()); }

        // Admin way geometry - move coords if no other consumer needs them
        if is_admin_way && !is_street && !is_building_addr && !is_interp {
            self.way_geom.insert(way_id, coords);
            return Ok(());
        }
        if is_admin_way {
            self.way_geom.insert(way_id, coords.clone());
        }

        // Interpolation ways - write nodes to file, keep slim metadata.
        // INVARIANT: `InterpWay.node_count` is u16 on disk (see `format.rs`).
        // OSM convention caps ways at 2000 refs; 65535 is well above that.
        // Hard-error rather than silently truncating the count while the
        // node-coordinate stream advances by the full length - that mismatch
        // used to truncate the tail of one way at read time.
        if is_interp {
            if coords.len() >= 2 {
                let itype = match interp.unwrap_or("") {
                    "even" => 1u8, "odd" => 2, _ => 0,
                };
                let nc = u16::try_from(coords.len()).map_err(|_| format!(
                    "interp way: {} coords exceeds u16::MAX. OSM ways are capped at \
                     2000 refs by convention; if this limit ever changes, bump \
                     `InterpWay.node_count` to u32 and increment FORMAT_VERSION.",
                    coords.len()
                ))?;
                // `start_number = 0, end_number = 0` is the unresolved sentinel
                // here - `resolve_interpolation_endpoints_mmap` will overwrite
                // these later if both endpoints match addr points. See the
                // KNOWN LIMITATION note on `SlimInterpWay` for the sentinel
                // ambiguity against interpolation ways that legitimately start
                // at house number 0.
                self.interp_ways.push(SlimInterpWay {
                    street_offset: self.strings.intern(addr_st.unwrap_or("")),
                    interpolation_type: itype,
                    node_file_offset: self.interp_node_offset,
                    node_count: nc,
                    start_number: 0,
                    end_number: 0,
                });
                for &(lat, lon) in &coords {
                    self.interp_nodes_out.write_all(
                        &NodeCoord { lat_e7: lat, lon_e7: lon }.to_bytes()
                    )?;
                }
                self.interp_node_offset += (coords.len() * NODE_COORD_SIZE) as u64;
            }
            return Ok(());
        }

        // Building addresses (centroid) - stream to addr_points.bin
        if is_building_addr {
            let (sum_lat, sum_lon) = coords.iter()
                .fold((0i64, 0i64), |acc, &(lat, lon)| {
                    (acc.0 + i64::from(lat), acc.1 + i64::from(lon))
                });
            #[allow(clippy::cast_possible_wrap)]
            let count = coords.len().max(1) as i64;
            #[allow(clippy::cast_possible_truncation)]
            let clat = (sum_lat / count) as i32;
            #[allow(clippy::cast_possible_truncation)]
            let clon = (sum_lon / count) as i32;
            let ap = AddrPoint {
                lat_e7: clat, lon_e7: clon,
                housenumber_offset: self.strings.intern(hn.unwrap_or("")),
                street_offset: self.strings.intern(addr_st.unwrap_or("")),
                postcode_offset: pc.map_or(0, |p| self.strings.intern(p)),
            };
            self.addr_points_out.write_all(&ap.to_bytes())?;
            if self.addr_point_count == 0 {
                self.first_addr_lat_e7 = clat;
                self.first_addr_lon_e7 = clon;
            }
            self.addr_point_count += 1;
        }

        // Streets - stream to street_ways.bin + street_nodes.bin.
        // INVARIANT: `StreetWay.node_count` is u16 on disk (see `format.rs`).
        // Hard-error on overflow - see the interp-way comment above for the
        // full rationale (silent truncation used to drop the tail of one way
        // at read time without any error signal).
        if is_street && coords.len() >= 2 {
            let nc = u16::try_from(coords.len()).map_err(|_| format!(
                "street way: {} coords exceeds u16::MAX. OSM ways are capped at \
                 2000 refs by convention; if this limit ever changes, bump \
                 `StreetWay.node_count` to u32 and increment FORMAT_VERSION.",
                coords.len()
            ))?;
            let sw = StreetWay {
                node_offset: self.street_node_offset,
                name_offset: self.strings.intern(name.unwrap_or("")),
                node_count: nc,
            };
            self.street_ways_out.write_all(&sw.to_bytes())?;
            for &(lat, lon) in &coords {
                self.street_nodes_out.write_all(
                    &NodeCoord { lat_e7: lat, lon_e7: lon }.to_bytes()
                )?;
            }
            self.street_node_offset += (coords.len() * NODE_COORD_SIZE) as u64;
            self.street_way_count += 1;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Pass 2 driver
// ---------------------------------------------------------------------------

/// Outputs of pass 2: counters, in-memory admin way geometry, interpolation
/// metadata, and mmaps of the four output files just written.
pub(super) struct Pass2Output {
    pub(super) addr_point_count: u32,
    pub(super) street_way_count: u32,
    pub(super) first_addr_lat_e7: i32,
    pub(super) first_addr_lon_e7: i32,
    pub(super) interp_ways: Vec<SlimInterpWay>,
    pub(super) way_geom: FxHashMap<i64, Vec<(i32, i32)>>,
    pub(super) street_ways_mmap: memmap2::Mmap,
    pub(super) street_nodes_mmap: memmap2::Mmap,
    pub(super) addr_points_mmap: memmap2::Mmap,
    pub(super) interp_nodes_mmap: memmap2::Mmap,
}

#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
pub(super) fn run_pass2(
    config: &BuildConfig,
    needed_admin_ways: crate::commands::id_set_dense::IdSetDense,
    mut referenced_nodes: crate::commands::id_set_dense::IdSetDense,
    strings: &mut StringPool,
) -> Result<Pass2Output> {
    let mut interp_ways: Vec<SlimInterpWay> = Vec::new();
    // Compact rank-indexed coord array. Instead of a 128 GB sparse DenseMmapIndex
    // (direct-addressed by node ID), allocate only referenced_count × 8 bytes and
    // index by rank. Writes are sequential (sorted PBF → monotonic ranks). Reads
    // during way processing have good locality (contiguous pages, not scattered).
    // Planet: ~16 GB contiguous vs ~83 GB scattered page cache.
    referenced_nodes.build_rank_index();
    let referenced_count = referenced_nodes.total_count();
    eprintln!("  {referenced_count} referenced nodes, compact index = {} MB",
        referenced_count * 8 / 1_000_000);
    #[allow(clippy::cast_possible_truncation)]
    let coord_array_len = referenced_count as usize * 8; // 8 bytes per (lat_e7: i32, lon_e7: i32)
    let mut coord_mmap = memmap2::MmapMut::map_anon(coord_array_len.max(1))?;
    let mut way_geom: rustc_hash::FxHashMap<i64, Vec<(i32, i32)>> = rustc_hash::FxHashMap::default();

    // Streaming output: write data files directly during the scan instead
    // of accumulating Vecs. Running counters track offsets and record counts.
    let mut street_ways_out = BufWriter::new(
        std::fs::File::create(config.output_dir.join(FILE_STREET_WAYS))?,
    );
    let mut street_nodes_out = BufWriter::new(
        std::fs::File::create(config.output_dir.join(FILE_STREET_NODES))?,
    );
    let mut addr_points_out = BufWriter::new(
        std::fs::File::create(config.output_dir.join(FILE_ADDR_POINTS))?,
    );
    let mut interp_nodes_out = BufWriter::new(
        std::fs::File::create(config.output_dir.join(FILE_INTERP_NODES))?,
    );

    let mut state = Pass2State {
        coord_mmap: &mut coord_mmap,
        referenced_nodes: &referenced_nodes,
        needed_admin_ways: &needed_admin_ways,
        way_geom: &mut way_geom,
        strings,
        interp_ways: &mut interp_ways,
        street_ways_out: &mut street_ways_out,
        street_nodes_out: &mut street_nodes_out,
        addr_points_out: &mut addr_points_out,
        interp_nodes_out: &mut interp_nodes_out,
        street_node_offset: 0,
        interp_node_offset: 0,
        addr_point_count: 0,
        street_way_count: 0,
        first_addr_lat_e7: 0,
        first_addr_lon_e7: 0,
    };

    {
        // Sequential reader to avoid PrimitiveBlock cross-thread alloc/free
        // retention (25+ GB at Europe/planet scale). The fused node+way scan
        // needs full PrimitiveBlock (for tag access), so we can't use the
        // node-only scanner here. But sequential decode keeps all alloc/free
        // on one thread, bounding heap to ~1.6 GB.
        // See notes/cross-pipeline-optimization-plan.md.
        let mut blob_reader = crate::blob::BlobReader::open(&config.input_path, config.direct_io)?;
        blob_reader.set_parse_indexdata(true);
        blob_reader.next()
            .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;
        let mut decompress_buf: Vec<u8> = Vec::new();
        let mut st_scratch: Vec<(u32, u32)> = Vec::new();
        let mut gr_scratch: Vec<(u32, u32)> = Vec::new();

        for blob_result in &mut blob_reader {
            let blob = blob_result?;
            if !matches!(blob.get_type(), crate::blob::BlobType::OsmData) {
                continue;
            }
            if let Some(idx) = blob.index() {
                if matches!(idx.kind, crate::blob_index::ElemKind::Relation) {
                    continue;
                }
            }
            blob.decompress_into(&mut decompress_buf)?;
            let block = crate::block::PrimitiveBlock::from_vec_with_scratch(
                std::mem::take(&mut decompress_buf), &mut st_scratch, &mut gr_scratch,
            )?;
            for element in block.elements_skip_metadata() {
                match element {
                    Element::DenseNode(node) => state.process_dense_node(&node)?,
                    Element::Way(way) => state.process_way(&way)?,
                    _ => {} // Node (non-dense) - rare, ignore
                }
            }
        } // for blob_result
    }

    let Pass2State {
        addr_point_count, street_way_count,
        first_addr_lat_e7, first_addr_lon_e7,
        ..
    } = state;

    // Flush and drop writers before mmap
    street_ways_out.flush()?;
    street_nodes_out.flush()?;
    addr_points_out.flush()?;
    interp_nodes_out.flush()?;
    drop(street_ways_out);
    drop(street_nodes_out);
    drop(addr_points_out);
    drop(interp_nodes_out);

    eprintln!("  {} addr, {} streets, {} interp, {} admin way geoms",
        addr_point_count, street_way_count, interp_ways.len(), way_geom.len());
    drop(needed_admin_ways);
    drop(referenced_nodes);
    drop(coord_mmap);

    // Mmap output files for coordinate access in cell assignment + interpolation
    // Mmap helper: handles empty files via anonymous read-only mmap (same as Reader).
    let mmap_file = |name: &str| -> Result<memmap2::Mmap> {
        let path = config.output_dir.join(name);
        let file = std::fs::File::open(&path)?;
        let len = file.metadata()?.len();
        if len == 0 {
            return Ok(
                memmap2::MmapOptions::new().map_anon()?.make_read_only()?
            );
        }
        Ok(unsafe { memmap2::Mmap::map(&file)? })
    };
    let street_ways_mmap = mmap_file(FILE_STREET_WAYS)?;
    let street_nodes_mmap = mmap_file(FILE_STREET_NODES)?;
    let addr_points_mmap = mmap_file(FILE_ADDR_POINTS)?;
    let interp_nodes_mmap = mmap_file(FILE_INTERP_NODES)?;

    Ok(Pass2Output {
        addr_point_count,
        street_way_count,
        first_addr_lat_e7,
        first_addr_lon_e7,
        interp_ways,
        way_geom,
        street_ways_mmap,
        street_nodes_mmap,
        addr_points_mmap,
        interp_nodes_mmap,
    })
}
