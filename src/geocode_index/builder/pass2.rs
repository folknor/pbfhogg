//! Pass 2: fused nodes + ways scan.
//!
//! Sorted PBFs (Sort.Type_then_ID) guarantee all node blobs come before
//! way blobs, so Pass 2 splits cleanly into a node phase (Phase 2a) and
//! a way phase (Phase 2b).
//!
//! - **Phase 2a** is parallel: workers decode node blobs via
//!   [`parallel_classify_phase`], build per-blob `Vec`s of (rank, lat, lon)
//!   coord-writes plus pending `AddrPoint`s (with owned strings), and
//!   send them to the main thread in blob-sequence order. The main thread
//!   applies coord writes to `coord_mmap` (safe because ranks are disjoint),
//!   interns strings into the shared `StringPool`, and streams `AddrPoint`s
//!   to `addr_points.bin`. The heavy decompression and decode work fans
//!   out across cores; string interning and `coord_mmap` writes stay on
//!   the main thread so no `StringPool` / mmap synchronisation is needed.
//!
//! - **Phase 2b** is still sequential: way blobs are processed in a single
//!   loop that classifies each way (street / building-addr / interp /
//!   admin-geometry), resolves coordinates via `referenced_nodes.rank()`
//!   into the `coord_mmap` populated by Phase 2a, and streams outputs to
//!   the street / interp / addr files plus the in-memory `way_geom` map.
//!   Way parallelisation is the next-layer follow-up (see
//!   `notes/geocode-build-opportunities.md` item #1 Phase 2b).

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
// Phase 2a: parallel node scan
// ---------------------------------------------------------------------------

/// Per-blob output from Phase 2a workers: pending `AddrPoint`s only —
/// coord writes go straight to `coord_mmap` from the worker (see
/// `CoordMmapShared` below), avoiding the 1.86 GB-on-Germany channel
/// traffic that the earlier "coord writes in NodeBlobOut" shape carried.
/// Strings stay owned (`Box<str>`) so the main thread interns them into
/// the shared `StringPool` at merge time — workers can't touch
/// `StringPool` directly without serialising on a mutex.
#[derive(Default)]
struct NodeBlobOut {
    addr_points: Vec<PendingAddrPoint>,
}

struct PendingAddrPoint {
    lat_e7: i32,
    lon_e7: i32,
    hn: Box<str>,
    st: Box<str>,
    pc: Option<Box<str>>,
}

/// Sync-safe wrapper around the `coord_mmap`'s raw pointer for Phase 2a
/// workers. Workers write `(lat_e7, lon_e7)` pairs at disjoint rank
/// offsets — the disjointness invariant follows from
/// `IdSetDense::rank(id)` being a unique index per set ID, combined with
/// sorted PBF guaranteeing every node ID appears in at most one blob.
/// No atomics needed because no two workers ever touch the same byte.
///
/// # Safety
///
/// The caller must guarantee:
/// - `ptr` remains valid and pointing at a `len`-byte allocation for
///   the entire lifetime of the `CoordMmapShared` value (callers hold
///   the owning `MmapMut` on the stack across the Phase 2a scope).
/// - Every `rank` value passed to `write_coord` satisfies
///   `rank * 8 + 8 <= len` — the per-blob `ref_rank_end` and
///   `IdSetDense::total_count()` together bound this.
/// - No two concurrent calls to `write_coord` pass the same `rank`.
struct CoordMmapShared {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: `ptr` is only dereferenced through `write_coord`, which writes
// 8 bytes at a disjoint offset per call. See the invariants on the
// struct docs.
unsafe impl Sync for CoordMmapShared {}
unsafe impl Send for CoordMmapShared {}

impl CoordMmapShared {
    /// Write `(lat_e7, lon_e7)` at the 8-byte slot `rank * 8`.
    ///
    /// # Safety
    ///
    /// See struct-level docs. Caller must ensure no two workers pass the
    /// same `rank` concurrently, and that `rank * 8 + 8 <= self.len`.
    unsafe fn write_coord(&self, rank: u64, lat_e7: i32, lon_e7: i32) {
        #[allow(clippy::cast_possible_truncation)]
        let off = (rank as usize) * 8;
        debug_assert!(off + 8 <= self.len,
            "CoordMmapShared: rank {rank} (offset {off}) out of bounds (len {})", self.len);
        let lat_bytes = lat_e7.to_le_bytes();
        let lon_bytes = lon_e7.to_le_bytes();
        // SAFETY: caller guarantees disjoint ranks and in-bounds `off`
        // (debug_assert above catches regressions during testing).
        unsafe {
            let p = self.ptr.add(off);
            std::ptr::copy_nonoverlapping(lat_bytes.as_ptr(), p, 4);
            std::ptr::copy_nonoverlapping(lon_bytes.as_ptr(), p.add(4), 4);
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 2b helper state (sequential way scan only)
// ---------------------------------------------------------------------------

/// Mutable state for the sequential Phase 2b way loop. Phase 2a has already
/// populated `coord_mmap` and `addr_points_out` with node-derived entries;
/// Phase 2b reads `coord_mmap` to resolve way refs and appends to
/// `addr_points_out` for building centroids.
struct Pass2bState<'a> {
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
    // Running counters (carried in from Phase 2a)
    street_node_offset: u64,
    interp_node_offset: u64,
    addr_point_count: u32,
    street_way_count: u32,
    first_addr_lat_e7: i32,
    first_addr_lon_e7: i32,
}

impl Pass2bState<'_> {
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

#[allow(clippy::too_many_lines, clippy::cognitive_complexity, clippy::too_many_arguments)]
#[hotpath::measure]
pub(super) fn run_pass2(
    config: &BuildConfig,
    node_schedule: &[(usize, u64, usize)],
    shared_file: &std::sync::Arc<std::fs::File>,
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
    crate::debug::emit_marker("GEOCODE_PASS2_RANK_INDEX_START");
    referenced_nodes.build_rank_index();
    crate::debug::emit_marker("GEOCODE_PASS2_RANK_INDEX_END");
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

    // State that Phase 2a populates and Phase 2b carries forward.
    let mut addr_point_count: u32 = 0;
    let mut first_addr_lat_e7: i32 = 0;
    let mut first_addr_lon_e7: i32 = 0;

    // ---------------- Phase 2a: parallel node scan ----------------
    //
    // Workers decode node blobs via `parallel_classify_phase`. Two outputs:
    //
    // 1. **Coord writes** land directly in `coord_mmap` from the worker
    //    via `CoordMmapShared::write_coord`. `IdSetDense::rank(id)` is
    //    unique per set ID, and sorted PBF guarantees each node ID
    //    appears in at most one blob, so workers never conflict on the
    //    same byte. Previously these writes flowed through the merge
    //    channel as a `Vec<(u64, i32, i32)>` per blob (1.86 GB of
    //    Germany traffic); moving them to the worker eliminates the
    //    main-thread serialisation bottleneck.
    //
    // 2. **Pending addr points** (addr-tagged nodes) flow to the main
    //    thread via the merge closure, which interns strings into the
    //    shared `StringPool` and streams `AddrPoint`s to
    //    `addr_points.bin` in blob-sequence order. Strings are owned
    //    (`Box<str>`) because `PrimitiveBlock` is dropped inside the
    //    worker — `&str` borrows into it would expire before merge.
    crate::debug::emit_marker("GEOCODE_PASS2A_START");
    {
        let referenced_ref = &referenced_nodes;
        let coord_shared = CoordMmapShared {
            ptr: coord_mmap.as_mut_ptr(),
            len: coord_mmap.len(),
        };
        let coord_ref = &coord_shared;
        let mut merge_err: Option<std::io::Error> = None;
        crate::commands::parallel_classify_phase(
            shared_file,
            node_schedule,
            NodeBlobOut::default,
            |block, state: &mut NodeBlobOut| -> NodeBlobOut {
                for element in block.elements_skip_metadata() {
                    if let Element::DenseNode(node) = element {
                        let lat_e7 = node.decimicro_lat();
                        let lon_e7 = node.decimicro_lon();
                        if let Some(rank) = referenced_ref.rank_if_set(node.id()) {
                            // SAFETY: disjoint ranks guaranteed by sorted PBF +
                            // unique rank-per-id in IdSetDense. See CoordMmapShared docs.
                            unsafe { coord_ref.write_coord(rank, lat_e7, lon_e7); }
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
                            state.addr_points.push(PendingAddrPoint {
                                lat_e7, lon_e7,
                                hn: h.into(),
                                st: s.into(),
                                pc: pc.map(Into::into),
                            });
                        }
                    }
                }
                std::mem::take(state)
            },
            |_seq, out| {
                if merge_err.is_some() { return; }
                for pap in out.addr_points {
                    let hn_off = strings.intern(&pap.hn);
                    let st_off = strings.intern(&pap.st);
                    let pc_off = pap.pc.as_deref().map_or(0, |s| strings.intern(s));
                    let ap = AddrPoint {
                        lat_e7: pap.lat_e7, lon_e7: pap.lon_e7,
                        housenumber_offset: hn_off,
                        street_offset: st_off,
                        postcode_offset: pc_off,
                    };
                    if let Err(e) = addr_points_out.write_all(&ap.to_bytes()) {
                        merge_err = Some(e);
                        return;
                    }
                    if addr_point_count == 0 {
                        first_addr_lat_e7 = pap.lat_e7;
                        first_addr_lon_e7 = pap.lon_e7;
                    }
                    addr_point_count += 1;
                }
            },
        )?;
        if let Some(e) = merge_err {
            return Err(e.into());
        }
    }
    crate::debug::emit_marker("GEOCODE_PASS2A_END");

    // ---------------- Phase 2b: sequential way scan ----------------
    //
    // Sequential decode of way blobs only. Ways read `coord_mmap` (populated
    // by Phase 2a) via `referenced_nodes.rank()` to resolve refs, and emit
    // to street / interp / addr / way_geom outputs. Kept sequential for now
    // — see `notes/geocode-build-opportunities.md` item #1 Phase 2b for
    // the planned parallel way scan (per-worker tmp files + offset-patched
    // concatenation for `street_ways.bin` / `street_nodes.bin` /
    // `interp_nodes.bin` / the way-local portion of `addr_points.bin`).
    let mut state = Pass2bState {
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
        addr_point_count,
        street_way_count: 0,
        first_addr_lat_e7,
        first_addr_lon_e7,
    };

    crate::debug::emit_marker("GEOCODE_PASS2B_SCAN_LOOP_START");
    {
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
            // Only process way blobs in Phase 2b; nodes handled by Phase 2a,
            // relations irrelevant to Pass 2.
            if let Some(idx) = blob.index() {
                if !matches!(idx.kind, crate::blob_index::ElemKind::Way) {
                    continue;
                }
            }
            blob.decompress_into(&mut decompress_buf)?;
            let block = crate::block::PrimitiveBlock::from_vec_with_scratch(
                std::mem::take(&mut decompress_buf), &mut st_scratch, &mut gr_scratch,
            )?;
            for element in block.elements_skip_metadata() {
                if let Element::Way(way) = element {
                    state.process_way(&way)?;
                }
            }
        }
    }
    crate::debug::emit_marker("GEOCODE_PASS2B_SCAN_LOOP_END");

    let Pass2bState {
        addr_point_count, street_way_count,
        first_addr_lat_e7, first_addr_lon_e7,
        ..
    } = state;

    crate::debug::emit_marker("GEOCODE_PASS2_FLUSH_MMAP_START");
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
    crate::debug::emit_marker("GEOCODE_PASS2_FLUSH_MMAP_END");

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
