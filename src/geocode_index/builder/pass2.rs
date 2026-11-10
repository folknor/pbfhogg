//! Pass 2: fused nodes + ways scan.
//!
//! Sorted PBFs (Sort.Type_then_ID) guarantee all node blobs come before
//! way blobs, so Pass 2 splits cleanly into a node phase (Phase 2a) and
//! a way phase (Phase 2b). Both are parallel.
//!
//! - **Phase 2a** (parallel nodes). Workers decode node blobs via
//!   `parallel_classify_phase_ordered`. Coord writes go directly into the
//!   coordinate store from the worker as relaxed compare-exchange installs
//!   (see [`coord_slots`] and `install_coord`). Every decoded node is checked against the node-ID
//!   bound Pass 1.5 trusted. Pending `AddrPoint`s flow to the main
//!   thread via the merge channel; the main thread interns strings
//!   into the shared `StringPool` and streams to `addr_points.bin`.
//!
//! - **Phase 2b** (parallel ways). The way blobs are not read again:
//!   workers decode each blob's segment of the way store Pass 1.5 wrote
//!   (`super::way_store`), already classified, resolve coords from the
//!   store populated by Phase 2a, and emit per-blob `WayBlobOut` records - `PendingStreet`,
//!   `PendingAddrPoint` (for building centroids), `PendingInterp`, and
//!   admin way geometry - with owned strings. Main-thread merge interns
//!   strings, writes records to `street_ways.bin` / `street_nodes.bin`
//!   / `addr_points.bin` / `interp_nodes.bin`, pushes `SlimInterpWay`
//!   entries, and inserts into the `way_geom` map.
//!
//! Both merges run in blob (file) order, Phase 2a through
//! `parallel_classify_phase_ordered` and Phase 2b through
//! `ordered_pool::run_ordered`; each holds dispatch at most
//! `GEOCODE_MERGE_WINDOW_BLOBS` ahead of the earliest unmerged blob, so a slow head
//! cannot grow the reorder buffer across the whole input, while merge still
//! overlaps classification. Duplicate admin ways use the last file
//! occurrence; conflicting coordinates for a referenced node fail the build.

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};

use rustc_hash::FxHashMap;

use crate::Element;

use super::Result;
use super::strings::StringPool;
use crate::error::Error;

use super::super::format::*;

// ---------------------------------------------------------------------------
// Intermediate data
// ---------------------------------------------------------------------------

/// Slim interpolation metadata kept in memory during the build.
/// Node coordinates are written directly to interp_nodes.bin; this struct
/// keeps the source way ID, file offset, count, and endpoint metadata.
/// Pass 2 pushes one per source way; the resolver replaces a resolved way
/// with one record per section between its address nodes, whose node spans
/// share the split node (see `interp.rs`).
///
/// Interpolation endpoints must have positive house numbers. Endpoint matching
/// rejects zero; serialized zero marks unresolved ways, so a real
/// interpolation endpoint at house number 0 is unsupported (see
/// CORRECTNESS.md). Fix shape if that is ever needed: persist a separate
/// `resolved: bool` into [`crate::geocode_index::format::InterpWay`] and bump
/// `FORMAT_VERSION`.
pub(super) struct SlimInterpWay {
    pub(super) way_id: i64,
    pub(super) node_file_offset: u64,
    /// What the way numbers along: from Pass 2, the way's own non-empty
    /// `addr:street`, else its `addr:place`, else `Street` at offset 0
    /// (none: the resolver infers one from the end nodes); after
    /// resolution, the resolved street or place.
    pub(super) parent_kind: ParentKind,
    pub(super) parent_offset: u32,
    /// The way's `addr:interpolation` mode; `None` for a value the index
    /// does not step (`alphabetic`, anything but `all`, `even`, `odd` and
    /// a digit 1 to 9), which is never resolved.
    pub(super) interpolation: Option<InterpolationType>,
    pub(super) node_count: u16,
    /// Every node ref of the source way resolved to a coordinate.
    pub(super) complete: bool,
    pub(super) start_number: u32,
    pub(super) end_number: u32,
}

// ---------------------------------------------------------------------------
// Phase 2a: parallel node scan
// ---------------------------------------------------------------------------

/// Per-blob output from Phase 2a workers: pending `AddrPoint`s only -
/// coord writes go straight to the coordinate store from the worker (see
/// `coord_slots` below), avoiding the 1.86 GB-on-Germany channel
/// traffic that the earlier "coord writes in NodeBlobOut" shape carried.
/// Strings stay owned (`Box<str>`) so the main thread interns them into
/// the shared `StringPool` at merge time - workers can't touch
/// `StringPool` directly without serialising on a mutex.
#[derive(Default)]
struct NodeBlobOut {
    addr_points: Vec<PendingAddrPoint>,
    error: Option<Error>,
}

/// Pending address point - used by both Phase 2a (addr-tagged nodes) and
/// Phase 2b (building centroids with addr tags). Owned strings so the
/// main thread can intern into the shared `StringPool` at merge time.
struct PendingAddrPoint {
    lat_e7: i32,
    lon_e7: i32,
    hn: Box<str>,
    st: Option<Box<str>>,
    place: Option<Box<str>>,
    pc: Option<Box<str>>,
}

/// The names an address carries, as `(street, place)`: an empty value is
/// no name, and an address must name a street, a place or both, else it is
/// not an address point. The one rule for nodes and buildings; offset 0 in
/// the index is the absent name it produces.
fn address_names<'t>(
    street: Option<&'t str>,
    place: Option<&'t str>,
) -> Option<(Option<&'t str>, Option<&'t str>)> {
    let street = street.filter(|s| !s.is_empty());
    let place = place.filter(|s| !s.is_empty());
    (street.is_some() || place.is_some()).then_some((street, place))
}

/// What an interpolation way says it numbers along: its non-empty
/// `addr:street`, else its non-empty `addr:place`, else nothing (the OSM
/// convention, inferred from the end nodes).
fn interpolation_parent<'t>(
    street: Option<&'t str>,
    place: Option<&'t str>,
) -> Option<(ParentKind, &'t str)> {
    match (
        street.filter(|s| !s.is_empty()),
        place.filter(|s| !s.is_empty()),
    ) {
        (Some(street), _) => Some((ParentKind::Street, street)),
        (None, Some(place)) => Some((ParentKind::Place, place)),
        (None, None) => None,
    }
}

/// Intern and append a blob's pending address points, in order. Both Pass
/// 2 merges run this serially for every address, so it is one out-of-line
/// loop rather than two inlined copies in the merge closures: inlined, the
/// Pass 2a merge became the planet bottleneck (Pass 2a 21 s to 30 s on
/// bygg; `reference/performance.md`, build-geocode-index).
#[inline(never)]
fn write_addr_points(
    points: Vec<PendingAddrPoint>,
    strings: &mut StringPool,
    out: &mut impl Write,
    count: &mut u32,
) -> Result<()> {
    for point in points {
        out.write_all(&point.intern(strings).to_bytes())?;
        *count = count.checked_add(1).ok_or_else(|| {
            Error::invalid_data("address point count exceeds u32::MAX; bump FORMAT_VERSION")
        })?;
    }
    Ok(())
}

impl PendingAddrPoint {
    fn intern(&self, strings: &mut StringPool) -> AddrPoint {
        let housenumber_offset = strings.intern(&self.hn);
        let street_offset = self.st.as_deref().map_or(0, |s| strings.intern(s));
        let postcode_offset = self.pc.as_deref().map_or(0, |s| strings.intern(s));
        let place_offset = self.place.as_deref().map_or(0, |s| strings.intern(s));
        AddrPoint {
            lat_e7: self.lat_e7,
            lon_e7: self.lon_e7,
            housenumber_offset,
            street_offset,
            postcode_offset,
            place_offset,
        }
    }
}

/// View the anonymous coordinate mmap as `slot_count` atomic slots, one per
/// referenced-node rank.
///
/// Phase 2a workers install into these slots concurrently and Phase 2b reads
/// them, both with `Relaxed` ordering; `parallel_classify_phase_ordered`
/// joins its workers before returning, which orders every 2a store before
/// every 2b load. The slots are atomic rather than plain bytes because nothing
/// guarantees two workers never write the same rank: that would need every
/// node ID to appear in at most one blob, and neither `Sort.Type_then_ID`
/// nor deduplication is checked on the way in (and `--force` skips even the
/// indexdata check). A node duplicated across blobs therefore makes two
/// workers store one slot; with atomics that is defined - one complete
/// coordinate is installed with compare-exchange; identical repeats are
/// accepted and conflicting repeats fail the build. Worker timing therefore
/// cannot select the coordinate used by a successful build.
///
/// The returned borrow is tied to the `&mut` borrow of the mmap, so no
/// other access to the mapping can overlap the view.
fn coord_slots(mmap: &mut memmap2::MmapMut, slot_count: usize) -> Result<&[AtomicU64]> {
    let needed = slot_count
        .checked_mul(std::mem::size_of::<AtomicU64>())
        .ok_or_else(|| {
            Error::internal(format!(
                "coordinate store of {slot_count} slots overflows usize"
            ))
        })?;
    if needed > mmap.len() {
        return Err(Error::internal(format!(
            "coordinate store mapping is {} bytes, {needed} needed for {slot_count} slots",
            mmap.len()
        )));
    }
    if slot_count == 0 {
        return Ok(&[]);
    }
    let base = mmap.as_mut_ptr();
    if base.align_offset(std::mem::align_of::<AtomicU64>()) != 0 {
        return Err(Error::internal(
            "coordinate store mapping is not aligned for AtomicU64",
        ));
    }
    // SAFETY: `base` is valid for reads and writes of `needed` bytes (checked
    // above against the mapping's length) and aligned for `AtomicU64`
    // (checked above; an mmap base is page-aligned). The bytes are
    // initialized (an anonymous mapping is zero-filled) and every bit
    // pattern is a valid `AtomicU64`, which has the size and bit validity of
    // `u64`. The slice lives no longer than the `&mut` borrow of the mapping,
    // so nothing else touches these bytes while it exists, and all access
    // through it is atomic.
    #[allow(clippy::cast_ptr_alignment)]
    let slots = unsafe { std::slice::from_raw_parts(base.cast::<AtomicU64>(), slot_count) };
    Ok(slots)
}

// The coordinate mmap spells absence with the crate's one slot encoding -
// latitude biased by 90 degrees so bit 31 is a free presence flag - rather
// than with an all-zero word. Spelling absence as all-zero used to make a
// node at Null Island indistinguishable from an unwritten slot, silently
// dropping the way that referenced it. The encoding is shared with the ALTW
// sparse store; see `crate::coord_pack` for the layout and its tests.
use crate::coord_pack::{pack as pack_coord, unpack as unpack_coord};

/// Slot for `rank`, or `None` when the rank falls outside the store. The
/// store is sized from the same `IdSet` that produces the ranks, so `None`
/// is a broken invariant; callers turn it into an error, never a write.
fn coord_slot(slots: &[AtomicU64], rank: u64) -> Option<&AtomicU64> {
    slots.get(usize::try_from(rank).ok()?)
}

/// Install a complete coordinate once. A successful build must not depend on
/// which worker encountered a duplicate referenced node first.
fn install_coord(slot: &AtomicU64, packed: u64) -> bool {
    match slot.compare_exchange(0, packed, Ordering::Relaxed, Ordering::Relaxed) {
        Ok(_) => true,
        Err(previous) => previous == packed,
    }
}

use crate::tunables::GEOCODE_MERGE_WINDOW_BLOBS;

// ---------------------------------------------------------------------------
// Phase 2b: parallel way scan
// ---------------------------------------------------------------------------

/// Per-blob output from Phase 2b workers. User strings are owned
/// (`Box<str>`), shared with Phase 2a's address points. Admin geometries are keyed by way ID for insertion
/// into the `way_geom` FxHashMap. The optional `error` field carries
/// u16-overflow diagnostics from `u16::try_from` (see
/// `StreetWay::node_count` / `InterpWay::node_count` invariants).
#[derive(Default)]
struct WayBlobOut {
    streets: Vec<PendingStreet>,
    building_addrs: Vec<PendingAddrPoint>,
    interps: Vec<PendingInterp>,
    admin_geoms: Vec<(i64, Vec<(i32, i32)>)>,
    missing_node_ref_count: u64,
    missing_ref_way_ids: Vec<i64>,
    error: Option<Error>,
}

struct PendingStreet {
    name: Box<str>,
    coords: Vec<(i32, i32)>,
}

struct PendingInterp {
    way_id: i64,
    parent: Option<(ParentKind, Box<str>)>,
    interpolation: Option<InterpolationType>,
    /// Every node ref resolved to a coordinate. A shortened way would join
    /// its surviving nodes across the gap and could put an interior address
    /// where an end was, so the resolver refuses an incomplete way.
    complete: bool,
    coords: Vec<(i32, i32)>,
}

/// Resolve one way-store record into `state`. Pure in effect (worker-local
/// state accumulation, no shared writes, no intern calls). Main thread
/// handles file I/O and string interning at merge time.
///
/// The record carries the classification Pass 1.5 made from the way's tags
/// (`pass1_5::extract_blob_ways`) and the tag values its roles read.
///
/// Coord resolution reads `coord_slots` (the store Phase 2a populated).
/// A ref is dropped from the way's geometry when its node was never
/// collected by Pass 1.5 or its slot was never written - the node is not in
/// the file. Absence is the `coord_pack` presence flag, not an all-zero
/// slot, so a node at Null Island (lat 0, lon 0) is kept like any other.
#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
fn resolve_way_into(
    way: &super::way_store::ExtractedWay<'_>,
    state: &mut WayBlobOut,
    referenced_nodes: &crate::idset::RankedIdSet,
    coord_slots: &[AtomicU64],
) {
    use super::way_store::{
        ROLE_ADMIN, ROLE_BUILDING_ADDR, ROLE_INTERP, ROLE_STREET, VALUE_HOUSENUMBER,
        VALUE_INTERPOLATION, VALUE_NAME, VALUE_PLACE, VALUE_POSTCODE, VALUE_STREET,
    };

    if state.error.is_some() {
        return;
    }

    let way_id = way.way_id;
    let is_admin_way = way.has(ROLE_ADMIN);
    let is_street = way.has(ROLE_STREET);
    let is_building_addr = way.has(ROLE_BUILDING_ADDR);
    let is_interp = way.has(ROLE_INTERP);
    let name = way.values[VALUE_NAME];
    let hn = way.values[VALUE_HOUSENUMBER];
    let addr_st = way.values[VALUE_STREET];
    let addr_place = way.values[VALUE_PLACE];
    let pc = way.values[VALUE_POSTCODE];
    let interp = way.values[VALUE_INTERPOLATION];

    if !is_admin_way && !is_street && !is_building_addr && !is_interp {
        return;
    }

    // Extracts can omit referenced nodes at their clip boundary. Use the
    // coordinates available in this PBF and shorten the way rather than
    // inventing a location or rejecting every other usable record. This is a
    // separate case from a decoded node whose coordinate is outside the legal
    // geographic domain, which cannot safely participate in the index.
    let mut rank_outside_store: Option<u64> = None;
    let mut missing_node_ref_count = 0_u64;
    let coords: Vec<(i32, i32)> = crate::read::elements::WayRefIter::delta_decoded(way.refs)
        .filter_map(|nid| {
            let Some(rank) = referenced_nodes.rank_if_set(nid) else {
                missing_node_ref_count = missing_node_ref_count.saturating_add(1);
                return None;
            };
            let Some(slot) = coord_slot(coord_slots, rank) else {
                rank_outside_store = Some(rank);
                return None;
            };
            let coord = unpack_coord(slot.load(Ordering::Relaxed));
            if coord.is_none() {
                missing_node_ref_count = missing_node_ref_count.saturating_add(1);
            }
            coord
        })
        .collect();
    if let Some(rank) = rank_outside_store {
        state.error = Some(Error::internal(format!(
            "internal error: way {way_id} resolves to coordinate rank {rank}, outside the \
             {}-slot store built from the same referenced-node set",
            coord_slots.len()
        )));
        return;
    }
    if missing_node_ref_count > 0 {
        state.missing_node_ref_count = state
            .missing_node_ref_count
            .saturating_add(missing_node_ref_count);
        super::sample_id(&mut state.missing_ref_way_ids, way_id);
    }
    if coords.is_empty() {
        return;
    }

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
            // Only the modes the index steps are accepted; reading any other
            // value as `all` would interpolate an alphabetic range as
            // consecutive integers.
            let interpolation = interp.and_then(InterpolationType::from_tag);
            if u16::try_from(coords.len()).is_err() {
                state.error = Some(Error::invalid_data(format!(
                    "interp way: {} coords exceeds u16::MAX. OSM ways are capped at \
                     2000 refs by convention; if this limit ever changes, bump \
                     `InterpWay.node_count` to u32 and increment FORMAT_VERSION.",
                    coords.len()
                )));
                return;
            }
            state.interps.push(PendingInterp {
                way_id,
                // An interpolation way tagged by the OSM convention carries
                // no street; the resolver takes it from the end nodes'
                // addresses.
                parent: interpolation_parent(addr_st, addr_place)
                    .map(|(kind, name)| (kind, name.into())),
                interpolation,
                complete: missing_node_ref_count == 0,
                coords,
            });
        }
        return;
    }

    // Building addresses (centroid) - main thread interns + streams.
    // The classifier saw presence; an address whose names are all empty is
    // no address point.
    let building_names = if is_building_addr {
        address_names(addr_st, addr_place)
    } else {
        None
    };
    if let Some((street, place)) = building_names {
        // This is the arithmetic mean of every resolved way coordinate. An
        // explicitly closed way counts its first vertex twice, and the mean
        // can fall outside a concave footprint. Replacing it with an area
        // centroid or an interior point changes stored address coordinates
        // and therefore query results; those methods also differ for concave
        // or malformed geometry. Keep the current placement until the
        // building-address point policy specifies which result is intended.
        let (sum_lat, sum_lon) = coords.iter().fold((0i64, 0i64), |acc, &(lat, lon)| {
            (acc.0 + i64::from(lat), acc.1 + i64::from(lon))
        });
        #[allow(clippy::cast_possible_wrap)]
        let count = coords.len().max(1) as i64;
        #[allow(clippy::cast_possible_truncation)]
        let clat = (sum_lat / count) as i32;
        #[allow(clippy::cast_possible_truncation)]
        let clon = (sum_lon / count) as i32;
        state.building_addrs.push(PendingAddrPoint {
            lat_e7: clat,
            lon_e7: clon,
            hn: hn.unwrap_or("").into(),
            st: street.map(Into::into),
            place: place.map(Into::into),
            pc: pc.map(Into::into),
        });
    }

    // Streets - defer file writes to merge. Same u16 invariant as interp.
    if is_street && coords.len() >= 2 {
        if u16::try_from(coords.len()).is_err() {
            state.error = Some(Error::invalid_data(format!(
                "street way: {} coords exceeds u16::MAX. OSM ways are capped at \
                 2000 refs by convention; if this limit ever changes, bump \
                 `StreetWay.node_count` to u32 and increment FORMAT_VERSION.",
                coords.len()
            )));
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
    pub(super) interp_ways: Vec<SlimInterpWay>,
    pub(super) way_geom: FxHashMap<i64, Vec<(i32, i32)>>,
    pub(super) street_ways: Records<StreetWay>,
    pub(super) street_nodes: Records<NodeCoord>,
    pub(super) addr_points: Records<AddrPoint>,
    pub(super) interp_nodes: Records<NodeCoord>,
}

#[allow(
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    clippy::too_many_arguments
)]
#[hotpath::measure]
pub(super) fn run_pass2(
    output_dir: &std::path::Path,
    node_schedule: &[crate::scan::classify::ScheduledBlob],
    way_schedule: &[crate::scan::classify::ScheduledBlob],
    way_store: &super::way_store::WayStore,
    shared_file: &crate::read::input::InputFile,
    max_node_id: i64,
    referenced_nodes: crate::idset::IdSet,
    strings: &mut StringPool,
    host: &crate::host::Entered<'_>,
) -> Result<Pass2Output> {
    let mut interp_ways: Vec<SlimInterpWay> = Vec::new();
    // Compact rank-indexed coord array. Instead of a 128 GB sparse DenseMmapIndex
    // (direct-addressed by node ID), allocate only referenced_count × 8 bytes and
    // index by rank. Writes are sequential (sorted PBF → monotonic ranks). Reads
    // during way processing have good locality (contiguous pages, not scattered).
    // Planet: ~16 GB contiguous vs ~83 GB scattered page cache.
    host.invocation.marker("GEOCODE_PASS2_RANK_INDEX_START");
    let referenced_nodes = referenced_nodes.into_ranked();
    host.invocation.marker("GEOCODE_PASS2_RANK_INDEX_END");
    let referenced_count = referenced_nodes.total_count();
    crate::host::diag!(
        host.diagnostics,
        Progress,
        "geocode.pass2.referenced_nodes",
        "  {referenced_count} referenced nodes, compact index = {} MB",
        referenced_count * 8 / 1_000_000
    );
    // One packed 8-byte coordinate slot per referenced node.
    let slot_count = usize::try_from(referenced_count).map_err(|_| {
        Error::invalid_data(format!("{referenced_count} referenced nodes exceed usize"))
    })?;
    let coord_array_len = slot_count
        .checked_mul(std::mem::size_of::<AtomicU64>())
        .ok_or_else(|| {
            Error::invalid_data(format!(
                "coordinate store for {referenced_count} nodes overflows usize"
            ))
        })?;
    // `map_anon` rejects a zero length; the view below still exposes zero
    // slots when there are no referenced nodes.
    let mut coord_mmap = memmap2::MmapMut::map_anon(coord_array_len.max(1))?;
    let coord_store: &[AtomicU64] = coord_slots(&mut coord_mmap, slot_count)?;
    let mut way_geom: rustc_hash::FxHashMap<i64, Vec<(i32, i32)>> =
        rustc_hash::FxHashMap::default();

    // Streaming output: write data files directly during the merge instead
    // of accumulating Vecs. Running counters track offsets and record counts.
    let mut street_ways_out = super::create_data_file(&output_dir.join(FILE_STREET_WAYS))?;
    let mut street_nodes_out = super::create_data_file(&output_dir.join(FILE_STREET_NODES))?;
    let mut addr_points_out = super::create_data_file(&output_dir.join(FILE_ADDR_POINTS))?;
    let mut interp_nodes_out = super::create_data_file(&output_dir.join(FILE_INTERP_NODES))?;

    // State that Phase 2a populates and Phase 2b carries forward.
    let mut addr_point_count: u32 = 0;

    // ---------------- Phase 2a: parallel node scan ----------------
    //
    // See module docs. Coord writes land in `coord_slots` directly from
    // workers; pending addr points flow to main-thread merge for string
    // interning + `addr_points.bin` write.
    host.invocation.marker("GEOCODE_PASS2A_START");
    {
        let referenced_ref = &referenced_nodes;
        crate::scan::classify::parallel_classify_phase_ordered(
            shared_file,
            node_schedule,
            None,
            std::num::NonZeroUsize::new(GEOCODE_MERGE_WINDOW_BLOBS),
            NodeBlobOut::default,
            |block, state: &mut NodeBlobOut| -> Result<NodeBlobOut> {
                for element in block.elements_skip_metadata() {
                    macro_rules! process_node {
                        ($node:expr) => {{
                            let node = $node;
                            let node_id = node.id();
                            // Pass 1.5 dropped every way ref above
                            // `max_node_id` as dangling. That is only sound
                            // if no node in the file is above it, so check
                            // every decoded node - before any other test can
                            // skip it - rather than trust the claim.
                            if node_id > max_node_id {
                                state.error = Some(crate::error::new_indexdata_mismatch(format!(
                                    "node {node_id} exceeds the node-ID bound {max_node_id} derived \
                                     from the blob metadata (the largest indexdata max_id over node \
                                     blobs, widened by any blob without indexdata). Some blob's \
                                     indexdata understates its ID range."
                                )));
                                continue;
                            }
                            let lat_e7 = node.decimicro_lat();
                            let lon_e7 = node.decimicro_lon();
                            let Some(packed) = pack_coord(lat_e7, lon_e7) else {
                                // An out-of-domain coordinate is decoded source
                                // data, not an absent reference at an extract
                                // edge. Silently skipping it could publish
                                // plausible but incomplete geometry, so this
                                // build remains fatal for that condition.
                                state.error = Some(Error::invalid_data(format!(
                                    "node {node_id} has coordinates outside the geographic domain (lat {lat_e7}, lon {lon_e7} in decimicrodegrees)"
                                )));
                                continue;
                            };
                            if let Some(rank) = referenced_ref.rank_if_set(node_id) {
                                let Some(slot) = coord_slot(coord_store, rank) else {
                                    state.error = Some(Error::internal(format!(
                                        "internal error: node {node_id} has coordinate rank {rank}, \
                                         outside the {}-slot store built from the same \
                                         referenced-node set",
                                        coord_store.len()
                                    )));
                                    continue;
                                };
                                if !install_coord(slot, packed) {
                                    state.error = Some(Error::invalid_data(format!(
                                        "node {node_id} has conflicting coordinates across occurrences"
                                    )));
                                    continue;
                                }
                            }
                            let mut hn: Option<&str> = None;
                            let mut st: Option<&str> = None;
                            let mut place: Option<&str> = None;
                            let mut pc: Option<&str> = None;
                            // Nearly every node tag is no `addr:` key, so one
                            // prefix test rejects it before the key match.
                            for (k, v) in node.tags() {
                                let Some(key) = k.strip_prefix("addr:") else {
                                    continue;
                                };
                                match key {
                                    "housenumber" => hn = Some(v),
                                    "street" => st = Some(v),
                                    "place" => place = Some(v),
                                    "postcode" => pc = Some(v),
                                    _ => {}
                                }
                            }
                            let names = hn.and_then(|h| Some((h, address_names(st, place)?)));
                            if let Some((h, (st, place))) = names {
                                state.addr_points.push(PendingAddrPoint {
                                    lat_e7,
                                    lon_e7,
                                    hn: h.into(),
                                    st: st.map(Into::into),
                                    place: place.map(Into::into),
                                    pc: pc.map(Into::into),
                                });
                            }
                        }};
                    }
                    match element {
                        Element::DenseNode(node) => process_node!(node),
                        Element::Node(node) => process_node!(node),
                        _ => {}
                    }
                }
                // A failure found in this blob ends the pool, located with
                // the blob it was found in.
                let mut out = std::mem::take(state);
                match out.error.take() {
                    Some(error) => Err(error),
                    None => Ok(out),
                }
            },
            |_seq, out| {
                write_addr_points(
                    out.addr_points,
                    strings,
                    &mut addr_points_out,
                    &mut addr_point_count,
                )?;
                Ok(())
            },
            host,
        )?;
    }
    host.invocation.marker("GEOCODE_PASS2A_END");

    // ---------------- Phase 2b: parallel way resolve ----------------
    //
    // One ordered task per scheduled way blob, empty ones included: a
    // worker decodes that blob's way-store segment, resolves coords via
    // the store populated by Phase 2a (only loaded from by this point;
    // Phase 2a's workers have joined), and emits a `WayBlobOut`. The
    // main-thread merge interns strings, writes records to the four output
    // files, pushes `SlimInterpWay` entries, and inserts into the `way_geom`
    // map. Merge order is way-schedule (file) order, through a bounded
    // window; see the module docs.
    let mut street_node_offset: u64 = 0;
    let mut interp_node_offset: u64 = 0;
    let mut street_way_count: u32 = 0;
    let mut missing_node_ref_count = 0_u64;
    let mut missing_ref_way_ids = Vec::new();

    host.invocation.marker("GEOCODE_PASS2B_START");
    {
        let referenced_ref = &referenced_nodes;
        let workers = crate::host::sized_workers(
            &*host.resources,
            crate::tunables::DECODE_WORKERS_RESERVED,
            crate::tunables::DECODE_WORKERS_MIN,
            crate::tunables::DECODE_WORKERS_FALLBACK,
        );
        let window = std::num::NonZeroUsize::new(GEOCODE_MERGE_WINDOW_BLOBS)
            .unwrap_or_else(|| crate::ordered_pool::default_window(workers));
        let mut shards = vec![(); workers];
        let resolve_blob = |state: &mut WayBlobOut, position: usize| -> Result<WayBlobOut> {
            let mut records = super::way_store::SegmentRecords::new(way_store.segment(position)?);
            while let Some(way) = records.next_way()? {
                resolve_way_into(&way, state, referenced_ref, coord_store);
            }
            let mut out = std::mem::take(state);
            match out.error.take() {
                Some(error) => Err(error),
                None => Ok(out),
            }
        };

        crate::ordered_pool::run_ordered(
            way_schedule.len(),
            &mut shards,
            crate::ordered_pool::PoolConfig {
                window,
                cancel: &host.invocation,
                waits: crate::ordered_pool::Waits::default(),
            },
            |_| WayBlobOut::default(),
            |(), state: &mut WayBlobOut, position| {
                resolve_blob(state, position).map_err(|e| match way_schedule.get(position) {
                    Some(entry) => shared_file.locate_blob(entry, e),
                    None => e,
                })
            },
            &mut crate::ordered_pool::ConsumeFn(|_seq: usize, out: WayBlobOut| -> Result<()> {
                missing_node_ref_count =
                    missing_node_ref_count.saturating_add(out.missing_node_ref_count);
                for &way_id in &out.missing_ref_way_ids {
                    super::sample_id(&mut missing_ref_way_ids, way_id);
                }

                // Admin way geometries
                for (way_id, coords) in out.admin_geoms {
                    way_geom.insert(way_id, coords);
                }

                // Interpolation ways: push SlimInterpWay + stream node coords.
                for pi in out.interps {
                    let nc = u16::try_from(pi.coords.len()).map_err(|_| {
                        Error::internal(format!(
                            "interp way coord count {} overflows u16 at merge (classify should have caught)",
                            pi.coords.len(),
                        ))
                    })?;
                    let (parent_kind, parent_offset) = match &pi.parent {
                        Some((kind, name)) => (*kind, strings.intern(name)),
                        None => (ParentKind::Street, 0),
                    };
                    interp_ways.push(SlimInterpWay {
                        way_id: pi.way_id,
                        node_file_offset: interp_node_offset,
                        parent_kind,
                        parent_offset,
                        interpolation: pi.interpolation,
                        node_count: nc,
                        complete: pi.complete,
                        // Zero marks endpoints that were not resolved.
                        start_number: 0,
                        end_number: 0,
                    });
                    for &(lat, lon) in &pi.coords {
                        interp_nodes_out.write_all(
                            &NodeCoord {
                                lat_e7: lat,
                                lon_e7: lon,
                            }
                            .to_bytes(),
                        )?;
                    }
                    interp_node_offset += (pi.coords.len() * NodeCoord::SIZE) as u64;
                }

                // Building-centroid addresses
                write_addr_points(
                    out.building_addrs,
                    strings,
                    &mut addr_points_out,
                    &mut addr_point_count,
                )?;

                // Streets
                for ps in out.streets {
                    let nc = u16::try_from(ps.coords.len()).map_err(|_| {
                        Error::internal(format!(
                            "street way coord count {} overflows u16 at merge (classify should have caught)",
                            ps.coords.len(),
                        ))
                    })?;
                    let name_off = strings.intern(&ps.name);
                    let sw = StreetWay {
                        node_offset: street_node_offset,
                        name_offset: name_off,
                        node_count: nc,
                    };
                    street_ways_out.write_all(&sw.to_bytes())?;
                    for &(lat, lon) in &ps.coords {
                        street_nodes_out.write_all(
                            &NodeCoord {
                                lat_e7: lat,
                                lon_e7: lon,
                            }
                            .to_bytes(),
                        )?;
                    }
                    street_node_offset += (ps.coords.len() * NodeCoord::SIZE) as u64;
                    street_way_count = street_way_count.checked_add(1).ok_or_else(|| {
                        Error::invalid_data(
                            "street way count exceeds u32::MAX; bump FORMAT_VERSION",
                        )
                    })?;
                }
                Ok(())
            }),
        )?;
    }
    host.invocation.marker("GEOCODE_PASS2B_END");

    host.invocation.counter(
        "geocode_way_refs_without_coordinates",
        missing_node_ref_count,
    );
    if missing_node_ref_count > 0 {
        crate::host::diag!(
            host.diagnostics,
            Warning,
            "geocode.pass2.missing_way_node_refs",
            "{missing_node_ref_count} geocode way node refs have no coordinate; \
             way IDs (sample): {missing_ref_way_ids:?}"
        );
    }

    host.invocation.marker("GEOCODE_PASS2_FLUSH_MMAP_START");
    // Flush and drop writers before mmap
    street_ways_out.flush()?;
    street_nodes_out.flush()?;
    addr_points_out.flush()?;
    interp_nodes_out.flush()?;
    drop(street_ways_out);
    drop(street_nodes_out);
    drop(addr_points_out);
    drop(interp_nodes_out);

    crate::host::diag!(
        host.diagnostics,
        Progress,
        "geocode.pass2.counts",
        "  {} addr, {} streets, {} interp, {} admin way geoms",
        addr_point_count,
        street_way_count,
        interp_ways.len(),
        way_geom.len()
    );
    drop(referenced_nodes);
    drop(coord_mmap);

    // Mmap output files for coordinate access in cell assignment + interpolation
    let street_ways = Records::map(&output_dir.join(FILE_STREET_WAYS))?;
    let street_nodes = Records::map(&output_dir.join(FILE_STREET_NODES))?;
    let addr_points = Records::map(&output_dir.join(FILE_ADDR_POINTS))?;
    let interp_nodes = Records::map(&output_dir.join(FILE_INTERP_NODES))?;
    host.invocation.marker("GEOCODE_PASS2_FLUSH_MMAP_END");

    Ok(Pass2Output {
        addr_point_count,
        street_way_count,
        interp_ways,
        way_geom,
        street_ways,
        street_nodes,
        addr_points,
        interp_nodes,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_coordinates_agree_or_fail_in_either_order() {
        let a = pack_coord(0, 0).unwrap();
        let b = pack_coord(1, 2).unwrap();
        for (first, second) in [(a, b), (b, a)] {
            let slot = AtomicU64::new(0);
            assert!(install_coord(&slot, first));
            assert!(install_coord(&slot, first));
            assert!(!install_coord(&slot, second));
            assert_eq!(slot.load(Ordering::Relaxed), first);
        }
    }
}
