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
//!   store populated by Phase 2a, and emit a per-blob `WayBlobOut` -
//!   streets (their `NodeCoord` records already encoded), building-centroid
//!   address points, `PendingInterp`s and admin way geometry, with the
//!   blob's strings in one buffer and their pool hashes precomputed, and
//!   each string the pool already held after Phase 2a resolved to its
//!   offset against a read-only clone of the pool. The
//!   main-thread merge interns the remaining strings, writes records to `street_ways.bin` / `street_nodes.bin`
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
/// Interpolation endpoints must have positive house numbers. Staged ways
/// start with zero endpoints and emit sections only after resolution.
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

/// Per-blob output from Phase 2a workers: pending address points only -
/// coord writes go straight to the coordinate store from the worker (see
/// `coord_slots` below), avoiding the 1.86 GB-on-Germany channel
/// traffic that the earlier "coord writes in NodeBlobOut" shape carried.
///
/// Phase 2a's merge is its bottleneck on planet (the consumer waits on
/// results under 4 % of the phase), so the output is packed as Phase 2b's
/// is: the blob's strings back to back in `text` with their pool hashes
/// computed here, rather than three or four `Box<str>` per point allocated
/// on a worker and freed on the merge thread. The merge interns them into
/// the shared `StringPool`, which workers cannot touch without serialising
/// on a mutex.
#[derive(Default)]
struct NodeBlobOut {
    text: String,
    addr_points: Vec<FlatAddrPoint>,
}

#[derive(Clone, Copy)]
enum AddressKey {
    HouseNumber,
    Street,
    Place,
    Postcode,
}

impl AddressKey {
    fn classify(key: &str) -> Option<Self> {
        match key {
            "addr:housenumber" => Some(Self::HouseNumber),
            "addr:street" => Some(Self::Street),
            "addr:place" => Some(Self::Place),
            "addr:postcode" => Some(Self::Postcode),
            _ => None,
        }
    }
}

fn classify_address_keys(
    block: &crate::PrimitiveBlock,
    classes: &mut Vec<Option<AddressKey>>,
) -> Result<()> {
    classes.clear();
    for sid in 0..block.string_table_len() {
        let key = block
            .string_table_entry(sid)
            .ok_or_else(|| Error::internal("validated node string table entry is missing"))?;
        classes.push(AddressKey::classify(key));
    }
    Ok(())
}

fn address_values<'a>(
    block: &'a crate::PrimitiveBlock,
    classes: &[Option<AddressKey>],
    tags: impl Iterator<Item = (usize, usize)>,
) -> Result<[Option<&'a str>; 4]> {
    let mut values = [None; 4];
    for (key, value) in tags {
        let class = classes.get(key).ok_or_else(|| {
            Error::internal("validated node tag key lies outside its string table")
        })?;
        let Some(class) = class else {
            continue;
        };
        let value = block.string_table_entry(value).ok_or_else(|| {
            Error::internal("validated node tag value lies outside its string table")
        })?;
        values[*class as usize] = Some(value);
    }
    Ok(values)
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

/// Intern and append a blob's address points, in order, each point's
/// strings in the order housenumber, street, postcode, place (the pool's
/// offsets follow first-occurrence order, so this order is part of the
/// index bytes; the order the strings sit in `text` is not). Both Pass 2
/// merges run this serially for every address, so it is one out-of-line
/// loop rather than two inlined copies in the merge closures: inlined, the
/// Pass 2a merge became the planet bottleneck (Pass 2a 21 s to 30 s on
/// bygg; `reference/performance/geocode.md`, build-geocode-index).
#[inline(never)]
fn write_addr_points(
    points: &[FlatAddrPoint],
    text: &str,
    strings: &mut StringPool,
    out: &mut impl Write,
    count: &mut u32,
) -> Result<()> {
    for point in points {
        let housenumber_offset = point.hn.intern(text, strings)?;
        let street_offset = match point.st {
            Some(s) => s.intern(text, strings)?,
            None => 0,
        };
        let postcode_offset = match point.pc {
            Some(s) => s.intern(text, strings)?,
            None => 0,
        };
        let place_offset = match point.place {
            Some(s) => s.intern(text, strings)?,
            None => 0,
        };
        let record = AddrPoint {
            lat_e7: point.lat_e7,
            lon_e7: point.lon_e7,
            housenumber_offset,
            street_offset,
            postcode_offset,
            place_offset,
        };
        out.write_all(&record.to_bytes())?;
        *count = count.checked_add(1).ok_or_else(|| {
            Error::invalid_data("address point count exceeds u32::MAX; bump FORMAT_VERSION")
        })?;
    }
    Ok(())
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

/// Per-blob output from Phase 2b workers.
///
/// Phase 2b is bound by its serial merge, not by its workers (they park on
/// the full result channel for most of the phase), so this is shaped to
/// leave the merge as little as possible: the strings of streets and
/// building addresses sit back to back in `text` with their pool hashes
/// computed here, and the street ways' `NodeCoord` records are encoded here
/// into `street_nodes`, which the merge writes with one call. A blob's
/// output is a handful of allocations, not several per way, so neither the
/// workers nor the merge (which frees them) spend the phase in the
/// allocator. Admin geometries are keyed by way ID for insertion into the
/// `way_geom` FxHashMap. The optional `error` field carries u16-overflow
/// diagnostics from `u16::try_from` (see `StreetWay::node_count` /
/// `InterpWay::node_count` invariants).
#[derive(Default)]
struct WayBlobOut {
    text: String,
    streets: Vec<FlatStreet>,
    street_nodes: Vec<u8>,
    building_addrs: Vec<FlatAddrPoint>,
    interps: Vec<PendingInterp>,
    /// Admin way occurrences in file order: the way ID and, for a fully
    /// located way, the ends of its packed refs in `admin_refs` and of its
    /// coordinates in `admin_coords` (each starting where the previous
    /// located way's ended).
    admin_geoms: Vec<(i64, Option<(usize, usize)>)>,
    admin_refs: Vec<u8>,
    admin_coords: Vec<(i32, i32)>,
    missing_node_ref_count: u64,
    missing_ref_way_ids: Vec<i64>,
    error: Option<Error>,
}

/// A string of a blob's packed `text` and its `StringPool` hash, or the
/// offset a worker already found for it.
#[derive(Clone, Copy)]
struct Text {
    start: usize,
    end: usize,
    hash: u64,
    /// The string's offset, found by the worker in a read-only clone of the
    /// pool taken before the phase (`StringPool::find_hashed`): final, since
    /// every entry of the clone is already in the live pool and keeps its
    /// offset. `None` leaves it to the merge, which interns it in order.
    resolved: Option<u32>,
}

impl Text {
    /// Append `s` to `text`, resolving it against `base` when given.
    fn push(text: &mut String, s: &str, base: Option<&StringPool>) -> Self {
        let start = text.len();
        text.push_str(s);
        let hash = super::strings::hash_str(s);
        Self {
            start,
            end: text.len(),
            hash,
            resolved: base.and_then(|pool| pool.find_hashed(s, hash)),
        }
    }

    fn intern(self, text: &str, strings: &mut StringPool) -> Result<u32> {
        if let Some(offset) = self.resolved {
            return Ok(offset);
        }
        let s = text
            .get(self.start..self.end)
            .ok_or_else(|| Error::internal("pass 2 text span lies outside its blob's text"))?;
        Ok(strings.intern_hashed(s, self.hash))
    }
}

/// A street way whose `node_count` coordinates are the next ones in
/// `WayBlobOut::street_nodes`.
struct FlatStreet {
    name: Text,
    node_count: u16,
}

/// An address point (an addressed node in Phase 2a, a building centroid in
/// Phase 2b), strings in its blob's packed `text`.
struct FlatAddrPoint {
    lat_e7: i32,
    lon_e7: i32,
    hn: Text,
    st: Option<Text>,
    place: Option<Text>,
    pc: Option<Text>,
}

/// One Phase 2b worker's state: the output under construction and the
/// coordinate buffer each way resolves into.
#[derive(Default)]
struct WayWorker {
    out: WayBlobOut,
    coords: Vec<(i32, i32)>,
    /// Indices into `coords` where a run of consecutively resolved refs
    /// ends because the next ref was unresolved. Empty for a complete way.
    run_breaks: Vec<usize>,
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
/// A ref is unresolved when its node was never collected by Pass 1.5 or its
/// slot was never written - the node is not in the file. Absence is the
/// `coord_pack` presence flag, not an all-zero slot, so a node at Null
/// Island (lat 0, lon 0) is kept like any other.
///
/// An unresolved ref never lets its neighbours join: the two refs either
/// side of it are not adjacent in the source, and an edge between them
/// would be invented geometry. So each consumer reads only what the
/// resolved refs prove:
///
/// - a street is split into its maximal runs of consecutively resolved
///   refs, one street record per run of two or more coordinates;
/// - a building address is the mean of its nodes, so an incomplete way
///   gives no address point;
/// - an interpolation way is staged with `complete == false` and the
///   resolver refuses it;
/// - an admin way that is incomplete is recorded as such (`None`), and
///   admin assembly withholds every relation that names it
///   (`admin::assemble_one_relation` says why a partial way cannot be
///   assembled around).
#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
fn resolve_way_into(
    way: &super::way_store::ExtractedWay<'_>,
    worker: &mut WayWorker,
    referenced_nodes: &crate::idset::RankedIdSet,
    coord_slots: &[AtomicU64],
    base: &StringPool,
) {
    let base = Some(base);
    use super::way_store::{
        ROLE_ADMIN, ROLE_BUILDING_ADDR, ROLE_INTERP, ROLE_STREET, VALUE_HOUSENUMBER,
        VALUE_INTERPOLATION, VALUE_NAME, VALUE_PLACE, VALUE_POSTCODE, VALUE_STREET,
    };

    let WayWorker {
        out: state,
        coords,
        run_breaks,
    } = worker;
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

    // Extracts can omit referenced nodes at their clip boundary. Such a ref
    // is not an error, but it is a gap: what each role keeps of the way is
    // stated on this function. This is a separate case from a decoded node
    // whose coordinate is outside the legal geographic domain, which cannot
    // safely participate in the index.
    let mut missing_node_ref_count = 0_u64;
    coords.clear();
    run_breaks.clear();
    for nid in crate::read::elements::WayRefIter::delta_decoded(way.refs) {
        let coord = match referenced_nodes.numeric_rank_if_set(nid) {
            None => None,
            Some(rank) => {
                let Some(slot) = coord_slot(coord_slots, rank) else {
                    state.error = Some(Error::internal(format!(
                        "internal error: way {way_id} resolves to coordinate rank {rank}, \
                         outside the {}-slot store built from the same referenced-node set",
                        coord_slots.len()
                    )));
                    return;
                };
                unpack_coord(slot.load(Ordering::Relaxed))
            }
        };
        match coord {
            Some(c) => coords.push(c),
            None => {
                missing_node_ref_count = missing_node_ref_count.saturating_add(1);
                if run_breaks.last() != Some(&coords.len()) {
                    run_breaks.push(coords.len());
                }
            }
        }
    }
    let coords: &[(i32, i32)] = coords;
    let complete = missing_node_ref_count == 0;
    if !complete {
        state.missing_node_ref_count = state
            .missing_node_ref_count
            .saturating_add(missing_node_ref_count);
        super::sample_id(&mut state.missing_ref_way_ids, way_id);
    }

    // Admin way geometry, staged in the blob's flat buffers for the merge
    // to copy into the `way_geom` store: the packed refs as the source has
    // them, and the coordinates. An incomplete way is recorded too, so the
    // last file occurrence of a duplicated way ID still decides, and
    // assembly sees why it has no geometry.
    if is_admin_way {
        let geometry = (complete && !coords.is_empty()).then(|| {
            state.admin_refs.extend_from_slice(way.refs);
            state.admin_coords.extend_from_slice(coords);
            (state.admin_refs.len(), state.admin_coords.len())
        });
        state.admin_geoms.push((way_id, geometry));
    }
    if coords.is_empty() {
        return;
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
                coords: coords.to_vec(),
            });
        }
        // An interpolation way is never a building address (it has no
        // `addr:housenumber`), but a named highway carrying
        // `addr:interpolation` is still a street and goes on to be one.
        if !is_street {
            return;
        }
    }

    // Building addresses (placed by `building_point`) - main thread interns
    // + streams.
    // The classifier saw presence; an address whose names are all empty is
    // no address point.
    // An incomplete footprint has no "mean of its nodes": the mean of the
    // survivors can land on a lone surviving node, where interpolation would
    // then read it as the address on that node.
    let building_names = if is_building_addr && complete {
        address_names(addr_st, addr_place)
    } else {
        None
    };
    let building_point = building_names.and_then(|names| Some((names, building_point(coords)?)));
    if let Some(((street, place), (clat, clon))) = building_point {
        let text = &mut state.text;
        let point = FlatAddrPoint {
            lat_e7: clat,
            lon_e7: clon,
            hn: Text::push(text, hn.unwrap_or(""), base),
            st: street.map(|s| Text::push(text, s, base)),
            place: place.map(|s| Text::push(text, s, base)),
            pc: pc.map(|s| Text::push(text, s, base)),
        };
        state.building_addrs.push(point);
    }

    // Streets - defer file writes to merge. Same u16 invariant as interp.
    // One record per run of consecutively resolved refs: a segment only ever
    // joins two nodes adjacent in the source way.
    if is_street {
        let mut start = 0;
        for end in run_breaks
            .iter()
            .copied()
            .chain(std::iter::once(coords.len()))
        {
            let run = &coords[start..end];
            start = end;
            if run.len() < 2 {
                continue;
            }
            let Ok(node_count) = u16::try_from(run.len()) else {
                state.error = Some(Error::invalid_data(format!(
                    "street way: {} coords exceeds u16::MAX. OSM ways are capped at \
                     2000 refs by convention; if this limit ever changes, bump \
                     `StreetWay.node_count` to u32 and increment FORMAT_VERSION.",
                    run.len()
                )));
                return;
            };
            let name = Text::push(&mut state.text, name.unwrap_or(""), base);
            state.street_nodes.reserve(run.len() * NodeCoord::SIZE);
            for &(lat_e7, lon_e7) in run {
                state
                    .street_nodes
                    .extend_from_slice(&NodeCoord { lat_e7, lon_e7 }.to_bytes());
            }
            state.streets.push(FlatStreet { name, node_count });
        }
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
    pub(super) way_geom: super::admin::WayGeometryMap,
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
    admin_totals: super::pass1_5::AdminWayTotals,
    strings: &mut StringPool,
    host: &crate::host::Entered<'_>,
) -> Result<Pass2Output> {
    let mut interp_ways: Vec<SlimInterpWay> = Vec::new();
    // Compact rank-indexed coord array. Instead of a 128 GB sparse DenseMmapIndex
    // (direct-addressed by node ID), allocate only referenced_count × 8 bytes and
    // index by numeric rank. On sorted positive input writes are sequential
    // (monotonic ranks); canonical order visits negatives 0, -1, -2, ...,
    // whose numeric ranks descend, which the compare-exchange slots accept in
    // any order. Reads
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
    let mut way_geom =
        super::admin::WayGeometryMap::with_capacity(admin_totals.ref_bytes, admin_totals.refs)?;

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
            || (Vec::new(), NodeBlobOut::default()),
            |block, (address_keys, state)| -> Result<NodeBlobOut> {
                // Dense validation in read/block.rs runs before this callback.
                // Reusing it preserves the array and metadata refusals. The
                // ALTW tuple scanner lacks tags and validates a different contract.
                classify_address_keys(block, address_keys)?;
                // Return the first node error in blob order to the pool,
                // which adds the blob location.
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
                                return Err(crate::error::new_indexdata_mismatch(format!(
                                    "node {node_id} exceeds the node-ID bound {max_node_id} derived \
                                     from the blob metadata (the largest indexdata max_id over node \
                                     blobs, widened by any blob without indexdata). Some blob's \
                                     indexdata understates its ID range."
                                )));
                            }
                            // The domain is judged on the exact nanodegree
                            // value, before truncation, as `check --locations`
                            // judges it. Judging the permissive
                            // `decimicro_lat` instead let a corrupt framing
                            // (raw latitude 4_294_967_296 at the default
                            // granularity) wrap to latitude 0 and be indexed
                            // there, and let 90.000000099 degrees truncate
                            // into the domain.
                            let (lat_nano, lon_nano) = (node.nano_lat_exact(), node.nano_lon_exact());
                            let in_domain = crate::coord_pack::decimicro_in_domain(lat_nano, lon_nano);
                            let Some(((lat_e7, lon_e7), packed)) = in_domain.and_then(|c| Some((c, pack_coord(c.0, c.1)?))) else {
                                // An out-of-domain coordinate is decoded source
                                // data, not an absent reference at an extract
                                // edge. Silently skipping it could publish
                                // plausible but incomplete geometry, so this
                                // build remains fatal for that condition.
                                return Err(Error::invalid_data(format!(
                                    "node {node_id} has coordinates outside the geographic domain (lat {lat_nano}, lon {lon_nano} in nanodegrees)"
                                )));
                            };
                            if let Some(rank) = referenced_ref.numeric_rank_if_set(node_id) {
                                let Some(slot) = coord_slot(coord_store, rank) else {
                                    return Err(Error::internal(format!(
                                        "internal error: node {node_id} has coordinate rank {rank}, \
                                         outside the {}-slot store built from the same \
                                         referenced-node set",
                                        coord_store.len()
                                    )));
                                };
                                if !install_coord(slot, packed) {
                                    return Err(Error::invalid_data(format!(
                                        "node {node_id} has conflicting coordinates across occurrences"
                                    )));
                                }
                            }
                            // Dense tag indices are signed on this API, and
                            // block validation already checked their bounds.
                            #[allow(clippy::cast_sign_loss)]
                            let tags = node.raw_tags().map(|(k, v)| (k as usize, v as usize));
                            let [hn, st, place, pc] = address_values(block, address_keys, tags)?;
                            let names = hn.and_then(|h| Some((h, address_names(st, place)?)));
                            if let Some((h, (st, place))) = names {
                                let text = &mut state.text;
                                let point = FlatAddrPoint {
                                    lat_e7,
                                    lon_e7,
                                    hn: Text::push(text, h, None),
                                    st: st.map(|s| Text::push(text, s, None)),
                                    place: place.map(|s| Text::push(text, s, None)),
                                    pc: pc.map(|s| Text::push(text, s, None)),
                                };
                                state.addr_points.push(point);
                            }
                        }};
                    }
                    match element {
                        Element::DenseNode(node) => process_node!(node),
                        Element::Node(node) => process_node!(node),
                        _ => {}
                    }
                }
                Ok(std::mem::take(state))
            },
            |_seq, out| {
                write_addr_points(
                    &out.addr_points,
                    &out.text,
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
        // Phase 2b is bound by its serial merge, most of which was interning
        // strings the pool already held from Phase 2a (house numbers,
        // streets, postcodes). Workers resolve those against this read-only
        // clone, taken after Phase 2a's workers have joined; the merge
        // interns only the rest, in its usual order, so every offset is the
        // one serial interning assigns. Freed when the phase ends.
        let base = strings.clone();
        let base_ref = &base;
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
        let resolve_blob = |worker: &mut WayWorker, position: usize| -> Result<WayBlobOut> {
            let mut records = super::way_store::SegmentRecords::new(way_store.segment(position)?);
            while let Some(way) = records.next_way()? {
                resolve_way_into(&way, worker, referenced_ref, coord_store, base_ref);
            }
            let mut out = std::mem::take(&mut worker.out);
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
            |_| WayWorker::default(),
            |(), worker: &mut WayWorker, position| {
                resolve_blob(worker, position).map_err(|e| match way_schedule.get(position) {
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

                // Admin way geometries, in file order.
                let (mut refs_start, mut coords_start) = (0, 0);
                for (way_id, ends) in out.admin_geoms {
                    let geometry = match ends {
                        None => None,
                        Some((refs_end, coords_end)) => {
                            let refs = out.admin_refs.get(refs_start..refs_end);
                            let coords = out.admin_coords.get(coords_start..coords_end);
                            (refs_start, coords_start) = (refs_end, coords_end);
                            match (refs, coords) {
                                (Some(r), Some(c)) => Some((r, c)),
                                _ => {
                                    return Err(Error::internal(format!(
                                        "phase 2b admin way {way_id} lies outside its blob's buffers"
                                    )));
                                }
                            }
                        }
                    };
                    way_geom.insert(way_id, geometry)?;
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

                // Building-centroid addresses, after the interpolation
                // parents and before the street names.
                let text = out.text.as_str();
                write_addr_points(
                    &out.building_addrs,
                    text,
                    strings,
                    &mut addr_points_out,
                    &mut addr_point_count,
                )?;

                // Streets: one record each, then the blob's coordinates in
                // one write, already encoded in way order.
                let mut blob_street_nodes = 0_u64;
                for street in &out.streets {
                    let name_offset = street.name.intern(text, strings)?;
                    let sw = StreetWay {
                        node_offset: street_node_offset,
                        name_offset,
                        node_count: street.node_count,
                    };
                    street_ways_out.write_all(&sw.to_bytes())?;
                    let bytes = u64::from(street.node_count) * NodeCoord::SIZE as u64;
                    street_node_offset += bytes;
                    blob_street_nodes += bytes;
                    street_way_count = street_way_count.checked_add(1).ok_or_else(|| {
                        Error::invalid_data(
                            "street way count exceeds u32::MAX; bump FORMAT_VERSION",
                        )
                    })?;
                }
                if blob_street_nodes != out.street_nodes.len() as u64 {
                    return Err(Error::internal(format!(
                        "phase 2b blob holds {} street node bytes for streets counting {blob_street_nodes}",
                        out.street_nodes.len()
                    )));
                }
                street_nodes_out.write_all(&out.street_nodes)?;
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

/// Where a complete building footprint's address point is stored, as
/// `(lat_e7, lon_e7)`, or `None` for a footprint without one.
///
/// The point is the arithmetic mean of every ref's coordinate, truncated
/// toward zero: an explicitly closed way counts its first vertex twice, and
/// the mean can fall outside a concave footprint. Both are deliberate. The
/// index answers nearest-address queries over footprints metres wide, so the
/// bias is metres, while an area centroid or interior point would move every
/// stored building point and with it the exact-coordinate interpolation
/// anchors; neither is a placement guarantee the contract makes.
///
/// Longitude is averaged in a frame where the footprint spans less than half
/// a turn. Raw longitudes are used when their span is below 180 degrees, which
/// keeps every such footprint, `-180` spellings included, exactly as it was.
/// Otherwise negative longitudes are lifted by a full turn, so a footprint
/// across the antimeridian averages near 180 degrees rather than near 0, and
/// the result is folded back. A footprint that spans half a turn or more in
/// both frames has no unique short arc and gets no address point.
fn building_point(coords: &[(i32, i32)]) -> Option<(i32, i32)> {
    const HALF_TURN: i64 = crate::geo::LON_FULL_TURN_E7 / 2;
    fn span(lons: impl Iterator<Item = i64>) -> i64 {
        let (lo, hi) = lons.fold((i64::MAX, i64::MIN), |(lo, hi), l| (lo.min(l), hi.max(l)));
        hi - lo
    }
    fn lift(lon: i32) -> i64 {
        let lon = i64::from(lon);
        if lon < 0 {
            lon + crate::geo::LON_FULL_TURN_E7
        } else {
            lon
        }
    }
    if coords.is_empty() {
        return None;
    }
    let raw = span(coords.iter().map(|&(_, lon)| i64::from(lon))) < HALF_TURN;
    if !raw && span(coords.iter().map(|&(_, lon)| lift(lon))) >= HALF_TURN {
        return None;
    }
    let (sum_lat, sum_lon) = coords.iter().fold((0i64, 0i64), |acc, &(lat, lon)| {
        let lon = if raw { i64::from(lon) } else { lift(lon) };
        (acc.0 + i64::from(lat), acc.1 + lon)
    });
    let count = i64::try_from(coords.len()).ok()?;
    let lon = sum_lon / count;
    let lon = if raw {
        lon
    } else {
        crate::geo::normalize_longitude_e7(lon)
    };
    Some((
        i32::try_from(sum_lat / count).ok()?,
        i32::try_from(lon).ok()?,
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn building_point_keeps_the_raw_mean_off_the_antimeridian() {
        // The closing vertex counts again, and truncation is toward zero.
        let ring = [(10, -100), (11, -101), (10, -103), (10, -100)];
        assert_eq!(building_point(&ring), Some((10, -101)));
        // Every ref at -180 keeps that spelling.
        let west = [(0, -1_800_000_000), (1, -1_800_000_000)];
        assert_eq!(building_point(&west), Some((0, -1_800_000_000)));
    }

    #[test]
    fn building_point_averages_across_the_antimeridian_on_the_short_arc() {
        let ring = [
            (-170_000_000, 1_799_999_000),
            (-170_000_000, -1_799_999_000),
            (-170_001_000, -1_799_999_000),
            (-170_001_000, 1_799_999_000),
            (-170_000_000, 1_799_999_000),
        ];
        // Lifted: three refs at 1_799_999_000, two at 1_800_001_000.
        assert_eq!(building_point(&ring), Some((-170_000_400, 1_799_999_800)));
        let east_of_seam = [(0, -1_799_999_000), (0, -1_799_999_000), (0, 1_799_999_999)];
        // Lifted mean 1_800_000_666 folds to -1_799_999_334.
        assert_eq!(building_point(&east_of_seam), Some((0, -1_799_999_334)));
    }

    #[test]
    fn building_point_refuses_a_footprint_without_a_short_arc() {
        assert_eq!(building_point(&[(0, 0), (0, 1_800_000_000)]), None);
        let wide = [
            (0, -1_000_000_000),
            (0, 0),
            (0, 1_000_000_000),
            (0, -1_000_000_000),
        ];
        assert_eq!(building_point(&wide), None);
        assert_eq!(building_point(&[]), None);
    }

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
