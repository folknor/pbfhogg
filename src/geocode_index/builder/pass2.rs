//! Pass 2: fused nodes + ways scan.
//!
//! Sorted PBFs (Sort.Type_then_ID) guarantee all node blobs come before
//! way blobs, so Pass 2 splits cleanly into a node phase (Phase 2a) and
//! a way phase (Phase 2b). Both are parallel.
//!
//! - **Phase 2a** (parallel nodes). Workers decode node blobs via
//!   [`parallel_classify_phase`]. Coord writes go directly into
//!   `coord_mmap` from the worker (disjoint ranks, no atomics needed -
//!   see [`CoordMmapShared`]). Pending `AddrPoint`s flow to the main
//!   thread via the merge channel; the main thread interns strings
//!   into the shared `StringPool` and streams to `addr_points.bin` in
//!   blob-sequence order.
//!
//! - **Phase 2b** (parallel ways). Workers decode way blobs, classify
//!   by tags, resolve coords from the mmap populated by Phase 2a, and
//!   emit per-blob `WayBlobOut` records - `PendingStreet`,
//!   `PendingAddrPoint` (for building centroids), `PendingInterp`, and
//!   admin way geometry - with owned strings. Main-thread merge interns
//!   strings, writes records to `street_ways.bin` / `street_nodes.bin`
//!   / `addr_points.bin` / `interp_nodes.bin`, pushes `SlimInterpWay`
//!   entries, and inserts into the `way_geom` map. The merge runs
//!   blob-sequence ordered so output files stay byte-stable.

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

/// Per-blob output from Phase 2a workers: pending `AddrPoint`s only -
/// coord writes go straight to `coord_mmap` from the worker (see
/// `CoordMmapShared` below), avoiding the 1.86 GB-on-Germany channel
/// traffic that the earlier "coord writes in NodeBlobOut" shape carried.
/// Strings stay owned (`Box<str>`) so the main thread interns them into
/// the shared `StringPool` at merge time - workers can't touch
/// `StringPool` directly without serialising on a mutex.
#[derive(Default)]
struct NodeBlobOut {
    addr_points: Vec<PendingAddrPoint>,
}

/// Pending address point - used by both Phase 2a (addr-tagged nodes) and
/// Phase 2b (building centroids with addr tags). Owned strings so the
/// main thread can intern into the shared `StringPool` at merge time.
struct PendingAddrPoint {
    lat_e7: i32,
    lon_e7: i32,
    hn: Box<str>,
    st: Box<str>,
    pc: Option<Box<str>>,
}

/// Sync-safe wrapper around the `coord_mmap`'s raw pointer for Phase 2a
/// workers. Workers write `(lat_e7, lon_e7)` pairs at disjoint rank
/// offsets - the disjointness invariant follows from
/// `IdSet::rank(id)` being a unique index per set ID, combined with
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
///   `rank * 8 + 8 <= len` - the per-blob `ref_rank_end` and
///   `IdSet::total_count()` together bound this.
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
// Phase 2b: parallel way scan
// ---------------------------------------------------------------------------

/// Per-blob output from Phase 2b workers. All user strings owned
/// (`Box<str>`) because the `PrimitiveBlock` is dropped on the worker
/// before the main thread sees the merge - any `&str` borrows into it
/// would expire. Admin geometries are keyed by way ID for insertion
/// into the `way_geom` FxHashMap. The optional `error` field carries
/// u16-overflow diagnostics from `u16::try_from` (see
/// `StreetWay::node_count` / `InterpWay::node_count` invariants).
#[derive(Default)]
struct WayBlobOut {
    streets: Vec<PendingStreet>,
    building_addrs: Vec<PendingAddrPoint>,
    interps: Vec<PendingInterp>,
    admin_geoms: Vec<(i64, Vec<(i32, i32)>)>,
    error: Option<String>,
}

struct PendingStreet {
    name: Box<str>,
    coords: Vec<(i32, i32)>,
}

struct PendingInterp {
    street: Box<str>,
    interpolation_type: u8,
    coords: Vec<(i32, i32)>,
}

/// Classify one way into `state`. Pure in effect (worker-local state
/// accumulation, no shared writes, no intern calls). Main thread handles
/// file I/O and string interning at merge time.
///
/// Coord resolution reads `coord_slice` (Phase 2a's populated
/// `coord_mmap` as a `&[u8]`); the zero-sentinel semantics (nodes at
/// lat==0 && lon==0 are dropped) match the previous sequential path.
#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
fn classify_way_into(
    way: &crate::elements::Way<'_>,
    state: &mut WayBlobOut,
    referenced_nodes: &crate::idset::IdSet,
    coord_slice: &[u8],
    needed_admin_ways: &crate::idset::IdSet,
) {
    if state.error.is_some() { return; }

    let way_id = way.id();
    let is_admin_way = needed_admin_ways.get(way_id);

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
        return;
    }

    let coords: Vec<(i32, i32)> = way.refs()
        .filter_map(|nid| {
            if !referenced_nodes.get(nid) { return None; }
            #[allow(clippy::cast_possible_truncation)]
            let r = referenced_nodes.rank(nid) as usize;
            let off = r * 8;
            let lat = i32::from_le_bytes(coord_slice.get(off..off+4)?.try_into().ok()?);
            let lon = i32::from_le_bytes(coord_slice.get(off+4..off+8)?.try_into().ok()?);
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
    if coords.is_empty() { return; }

    // Admin way geometry - move coords if no other consumer needs them
    if is_admin_way && !is_street && !is_building_addr && !is_interp {
        state.admin_geoms.push((way_id, coords));
        return;
    }
    if is_admin_way {
        state.admin_geoms.push((way_id, coords.clone()));
    }

    // Interpolation ways - defer file writes to merge, keep slim
    // per-blob metadata. INVARIANT check mirrors the sequential
    // path's `u16::try_from` - worker sets `state.error` on overflow.
    if is_interp {
        if coords.len() >= 2 {
            let itype = match interp.unwrap_or("") {
                "even" => 1u8, "odd" => 2, _ => 0,
            };
            if u16::try_from(coords.len()).is_err() {
                state.error = Some(format!(
                    "interp way: {} coords exceeds u16::MAX. OSM ways are capped at \
                     2000 refs by convention; if this limit ever changes, bump \
                     `InterpWay.node_count` to u32 and increment FORMAT_VERSION.",
                    coords.len()
                ));
                return;
            }
            state.interps.push(PendingInterp {
                street: addr_st.unwrap_or("").into(),
                interpolation_type: itype,
                coords,
            });
        }
        return;
    }

    // Building addresses (centroid) - main thread interns + streams.
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
        state.building_addrs.push(PendingAddrPoint {
            lat_e7: clat, lon_e7: clon,
            hn: hn.unwrap_or("").into(),
            st: addr_st.unwrap_or("").into(),
            pc: pc.map(Into::into),
        });
    }

    // Streets - defer file writes to merge. Same u16 invariant as interp.
    if is_street && coords.len() >= 2 {
        if u16::try_from(coords.len()).is_err() {
            state.error = Some(format!(
                "street way: {} coords exceeds u16::MAX. OSM ways are capped at \
                 2000 refs by convention; if this limit ever changes, bump \
                 `StreetWay.node_count` to u32 and increment FORMAT_VERSION.",
                coords.len()
            ));
            return;
        }
        state.streets.push(PendingStreet {
            name: name.unwrap_or("").into(),
            coords,
        });
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
    way_schedule: &[(usize, u64, usize)],
    shared_file: &std::sync::Arc<std::fs::File>,
    needed_admin_ways: crate::idset::IdSet,
    mut referenced_nodes: crate::idset::IdSet,
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

    // Streaming output: write data files directly during the merge instead
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
    // See module docs. Coord writes land in `coord_mmap` directly from
    // workers; pending addr points flow to main-thread merge for string
    // interning + `addr_points.bin` write.
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
                            // unique rank-per-id in IdSet. See CoordMmapShared docs.
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

    // ---------------- Phase 2b: parallel way scan ----------------
    //
    // Workers classify each way, resolve coords via the mmap populated
    // by Phase 2a (read-only by this point), and emit a `WayBlobOut` per
    // blob. The main-thread merge interns strings, writes records to the
    // four output files, pushes `SlimInterpWay` entries, and inserts
    // into the `way_geom` map. Merge runs in blob-sequence order via
    // `parallel_classify_phase`'s internal ReorderBuffer so the output
    // files stay byte-stable.
    let mut street_node_offset: u64 = 0;
    let mut interp_node_offset: u64 = 0;
    let mut street_way_count: u32 = 0;

    crate::debug::emit_marker("GEOCODE_PASS2B_START");
    {
        let referenced_ref = &referenced_nodes;
        let coord_slice: &[u8] = &coord_mmap[..];
        let needed_admin_ways_ref = &needed_admin_ways;
        let mut merge_err: Option<String> = None;

        crate::commands::parallel_classify_phase(
            shared_file,
            way_schedule,
            WayBlobOut::default,
            |block, state: &mut WayBlobOut| -> WayBlobOut {
                for element in block.elements_skip_metadata() {
                    if let Element::Way(way) = element {
                        classify_way_into(
                            &way, state, referenced_ref, coord_slice, needed_admin_ways_ref,
                        );
                    }
                }
                std::mem::take(state)
            },
            |_seq, out| {
                if merge_err.is_some() { return; }
                if let Some(err) = out.error {
                    merge_err = Some(err);
                    return;
                }

                // Admin way geometries
                for (way_id, coords) in out.admin_geoms {
                    way_geom.insert(way_id, coords);
                }

                // Interpolation ways: push SlimInterpWay + stream node coords.
                for pi in out.interps {
                    let nc = match u16::try_from(pi.coords.len()) {
                        Ok(n) => n,
                        Err(_) => {
                            merge_err = Some(format!(
                                "interp way coord count {} overflows u16 at merge (classify should have caught)",
                                pi.coords.len(),
                            ));
                            return;
                        }
                    };
                    let street_offset = strings.intern(&pi.street);
                    interp_ways.push(SlimInterpWay {
                        street_offset,
                        interpolation_type: pi.interpolation_type,
                        node_file_offset: interp_node_offset,
                        node_count: nc,
                        // `(start_number = 0, end_number = 0)` is the unresolved
                        // sentinel - `resolve_interpolation_endpoints_mmap`
                        // overwrites when both endpoints match addr points.
                        // KNOWN LIMITATION on `SlimInterpWay` applies.
                        start_number: 0,
                        end_number: 0,
                    });
                    for &(lat, lon) in &pi.coords {
                        if let Err(e) = interp_nodes_out.write_all(
                            &NodeCoord { lat_e7: lat, lon_e7: lon }.to_bytes()
                        ) {
                            merge_err = Some(e.to_string());
                            return;
                        }
                    }
                    interp_node_offset += (pi.coords.len() * NODE_COORD_SIZE) as u64;
                }

                // Building-centroid addresses
                for pap in out.building_addrs {
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
                        merge_err = Some(e.to_string());
                        return;
                    }
                    if addr_point_count == 0 {
                        first_addr_lat_e7 = pap.lat_e7;
                        first_addr_lon_e7 = pap.lon_e7;
                    }
                    addr_point_count += 1;
                }

                // Streets
                for ps in out.streets {
                    let nc = match u16::try_from(ps.coords.len()) {
                        Ok(n) => n,
                        Err(_) => {
                            merge_err = Some(format!(
                                "street way coord count {} overflows u16 at merge (classify should have caught)",
                                ps.coords.len(),
                            ));
                            return;
                        }
                    };
                    let name_off = strings.intern(&ps.name);
                    let sw = StreetWay {
                        node_offset: street_node_offset,
                        name_offset: name_off,
                        node_count: nc,
                    };
                    if let Err(e) = street_ways_out.write_all(&sw.to_bytes()) {
                        merge_err = Some(e.to_string());
                        return;
                    }
                    for &(lat, lon) in &ps.coords {
                        if let Err(e) = street_nodes_out.write_all(
                            &NodeCoord { lat_e7: lat, lon_e7: lon }.to_bytes()
                        ) {
                            merge_err = Some(e.to_string());
                            return;
                        }
                    }
                    street_node_offset += (ps.coords.len() * NODE_COORD_SIZE) as u64;
                    street_way_count += 1;
                }
            },
        )?;
        if let Some(e) = merge_err {
            return Err(e.into());
        }
    }
    crate::debug::emit_marker("GEOCODE_PASS2B_END");

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
