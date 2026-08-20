//! Rank-indexed flat node coordinate store.
//!
//! Pre-allocates a `referenced.total_count() * 8`-byte temp file and
//! mmaps it `MmapMut`. Pass 1 workers write `(lat, lon)` at byte offset
//! `rank_if_set(node_id) << 3` via `AtomicU64::store(Relaxed)` directly
//! into the mmap; pass 2's `get(node_id)` is `rank_if_set(node_id)` plus
//! an `AtomicU64::load(Relaxed)` at the same offset. No chunk format,
//! no `start_pad`, no sentinel padding inside chunks (unwritten slots
//! stay zero, which is the existing sentinel).
//!
//! Trade-offs vs the previous chunk-indexed encoding (Planetiler-style):
//! - Disk shrinks ~2.4x at japan density (5.7 GB -> 2.4 GB; 8 bytes /
//!   referenced node, no chunk-padding overhead).
//! - Pass 1 becomes parallel: workers write directly via mmap with no
//!   serial consumer, so the dispatcher / merge bottleneck disappears.
//! - The strictly-increasing-id precondition is gone. Random insertion
//!   order works because each rank slot is unique and the AtomicU64
//!   stores are race-free per slot.
//! - At the cost of carrying the IdSet (and its rank index) into pass 2:
//!   ~440 MB + ~100 MB at planet, vs the chunk format's ~440 MB
//!   `offsets`+`start_pad`. Net RAM is roughly flat.
//!
//! # Absence is a flag bit, not a coordinate
//!
//! "Referenced but never written" used to be spelled `(lat, lon) == (0, 0)`,
//! which is Null Island - a real, legal OSM coordinate. A node genuinely at
//! `0, 0` was reported missing, and a missing node was written into a way as
//! if it sat in the Gulf of Guinea. All-zero as an absence sentinel is the
//! same mistake indexdata v3 already corrected by spelling an absent bbox as
//! an inverted box rather than an all-zero one.
//!
//! The slot is still eight bytes and `set_len` still zero-fills, so an
//! untouched slot still reads as absent - but presence is now carried by an
//! explicit bit that no coordinate can set by accident. Latitude is stored
//! biased by `LAT_BIAS`, which puts every legal latitude in `0 ..= 1.8e9`,
//! below `2^31`; bit 31 is therefore free and means "written". Longitude
//! occupies the upper word verbatim. See [`pack`] / [`unpack`].

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::idset::IdSet;

use super::Result;

#[cfg(target_os = "linux")]
fn preallocate_sparse_file(file: &std::fs::File, scratch_dir: &Path, bytes: u64) -> Result<()> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd as _;
    use std::os::unix::ffi::OsStrExt as _;

    let path = CString::new(scratch_dir.as_os_str().as_bytes())
        .map_err(|_| "scratch directory path contains a NUL byte")?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // Safety: `path` is NUL-terminated and `stats` points to writable storage.
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(format!(
            "failed to query free space for sparse index in {}: {}",
            scratch_dir.display(),
            std::io::Error::last_os_error()
        )
        .into());
    }
    // Safety: statvfs initialized `stats` on success.
    let stats = unsafe { stats.assume_init() };
    let available = stats.f_bavail.saturating_mul(stats.f_frsize);
    if available < bytes {
        return Err(format!(
            "insufficient free space for sparse node index: need {bytes} bytes, have {available} bytes in {}",
            scratch_dir.display()
        )
        .into());
    }
    let length: libc::off_t = bytes
        .try_into()
        .map_err(|_| "sparse index size does not fit the platform file-offset type")?;
    // Safety: `file` owns a valid writable descriptor and length is nonnegative.
    let rc = unsafe { libc::posix_fallocate(file.as_raw_fd(), 0, length) };
    if rc != 0 {
        return Err(format!(
            "failed to allocate {bytes} bytes for sparse node index: {}",
            std::io::Error::from_raw_os_error(rc)
        )
        .into());
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn preallocate_sparse_file(file: &std::fs::File, _scratch_dir: &Path, bytes: u64) -> Result<()> {
    file.set_len(bytes)
        .map_err(|e| format!("failed to size sparse index file ({bytes} bytes): {e}").into())
}

/// Create, preallocate and immediately unlink the sparse node store, handing
/// back the still-open descriptor.
///
/// The file is PID-named and therefore reachable by name for the window
/// between `create_new` and the unlink. Preallocation can fail inside that
/// window *after* `posix_fallocate` has reserved blocks, so the window is
/// covered by a `PathGuard`: without it a failed run leaves a partially
/// allocated, potentially multi-gigabyte file in scratch until some later
/// run happens to be assigned the same PID and clears it. The guard is
/// committed only once the unlink has succeeded, at which point the path is
/// already gone.
fn open_anonymous_sparse_file(scratch_dir: &Path, total_bytes: u64) -> Result<std::fs::File> {
    let temp_path = scratch_dir.join(format!(".pbfhogg-sparse-index-{}", std::process::id()));
    match std::fs::remove_file(&temp_path) {
        Ok(()) => eprintln!(
            "removed stale sparse index temp file {}",
            temp_path.display()
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(format!(
                "failed to remove stale sparse index temp file {}: {e}",
                temp_path.display()
            )
            .into());
        }
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .map_err(|e| format!("failed to create sparse index temp file: {e}"))?;
    let guard = crate::path_guard::PathGuard::file(temp_path.clone());
    preallocate_sparse_file(&file, scratch_dir, total_bytes)?;
    std::fs::remove_file(&temp_path).map_err(|e| {
        format!(
            "failed to unlink sparse index temp file {} after allocation: {e}",
            temp_path.display()
        )
    })?;
    drop(guard.commit());
    Ok(file)
}

/// Latitude bias, in decimicrodegrees: 90 degrees.
const LAT_BIAS: i64 = 900_000_000;
/// Presence flag. Set on every written slot; unreachable by any biased
/// latitude, since the biased range tops out at `1.8e9 < 2^31`.
const PRESENT: u64 = 1 << 31;
/// Longitude bound in decimicrodegrees: 180 degrees.
const LON_LIMIT: i64 = 1_800_000_000;

/// Pack a geographic coordinate into one slot word, with the presence bit
/// set. Returns `None` when the coordinate is outside the geographic domain
/// and therefore not representable - the caller must refuse rather than
/// store a value that would decode as some other place.
///
/// The range check is deliberately geographic, not merely representational:
/// every `(i32, i32)` pair is representable in 64 bits, but only latitudes
/// within +/-90 and longitudes within +/-180 are coordinates.
fn pack(lat: i32, lon: i32) -> Option<u64> {
    let lat = i64::from(lat);
    let lon = i64::from(lon);
    if !(-LAT_BIAS..=LAT_BIAS).contains(&lat) || !(-LON_LIMIT..=LON_LIMIT).contains(&lon) {
        return None;
    }
    #[allow(clippy::cast_sign_loss)]
    let biased_lat = (lat + LAT_BIAS) as u64;
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let lon_bits = u64::from(lon as i32 as u32);
    Some(biased_lat | PRESENT | (lon_bits << 32))
}

/// Inverse of [`pack`]. `None` means the slot was never written.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
fn unpack(packed: u64) -> Option<(i32, i32)> {
    if packed & PRESENT == 0 {
        return None;
    }
    let lat = ((packed & 0x7fff_ffff) as i64 - LAT_BIAS) as i32;
    let lon = (packed >> 32) as u32 as i32;
    Some((lat, lon))
}

/// Rank-indexed flat node coordinate store.
pub(super) struct SparseArrayIndex {
    /// Carries the `referenced` IdSet plus its rank index. `get` does
    /// `rank_if_set(node_id) -> Some(rank)` then an `AtomicU64::load`
    /// at `rank * 8`.
    referenced: IdSet,
    mmap: memmap2::MmapMut,
    _file: std::fs::File,
}

impl SparseArrayIndex {
    #[allow(clippy::cast_possible_truncation)]
    pub(super) fn get(&self, node_id: i64) -> Option<(i32, i32)> {
        let rank = self.referenced.rank_if_set(node_id)?;
        let byte_offset = (rank * 8) as usize;
        // Not a "node absent" case: `rank_if_set` just said the id is in the
        // IdSet the store was sized from, so a rank past the end is a broken
        // invariant, not a miss. Reporting `None` here made a store/IdSet
        // mismatch indistinguishable from an unreferenced node - it became a
        // missing location and then a (0, 0) coordinate in the output.
        assert!(
            byte_offset + 8 <= self.mmap.len(),
            "sparse index rank {rank} for node {node_id} is outside the {} \
             byte store built from the same IdSet",
            self.mmap.len()
        );
        // SAFETY: byte_offset + 8 <= mmap.len(), pointer is 8-byte aligned
        // (page-aligned base + 8*rank). Atomic load pairs with atomic
        // stores in `SharedSparseWriter::insert` to eliminate data-race UB.
        let packed = unsafe {
            let ptr = self.mmap.as_ptr().add(byte_offset).cast::<AtomicU64>();
            (*ptr).load(Ordering::Relaxed)
        };
        unpack(packed)
    }
}

/// Thread-safe writer for parallel sparse-rank index population.
///
/// Holds a raw pointer into the `SparseArrayIndex` mmap buffer. Each
/// referenced node id maps to a disjoint 8-byte slot
/// (`base + rank_if_set(id) * 8`). All writes use
/// `AtomicU64::store(Relaxed)`, eliminating data-race UB even if
/// duplicate node IDs appear in the input.
struct SharedSparseWriter {
    base: *mut u8,
    capacity_bytes: usize,
}

// SAFETY: All writes use atomic operations (AtomicU64 stores). The raw
// pointer requires manual Send+Sync; lifetime is bounded by the
// synchronous parallel scan in `build_node_index_sparse`.
unsafe impl Send for SharedSparseWriter {}
unsafe impl Sync for SharedSparseWriter {}

impl SharedSparseWriter {
    /// Insert a referenced node's coordinates at byte offset `rank * 8`.
    ///
    /// Errors when the coordinate is outside the geographic domain, because
    /// the slot encoding cannot represent it and storing a truncated value
    /// would put the node somewhere else on Earth without saying so.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn insert(&self, node_id: i64, rank: u64, lat: i32, lon: i32) -> crate::error::Result<()> {
        let packed = pack(lat, lon).ok_or_else(|| {
            crate::error::new_error(crate::error::ErrorKind::Io(std::io::Error::other(format!(
                "node {node_id} has coordinates outside the geographic domain \
                 (lat {lat}, lon {lon} in decimicrodegrees; latitude must be \
                 within +/-90 degrees and longitude within +/-180). Refusing \
                 to build a node index from it."
            ))))
        })?;
        let byte_offset = (rank * 8) as usize;
        assert!(
            byte_offset + 8 <= self.capacity_bytes,
            "sparse index rank {rank} is outside the {} byte store built from \
             the same IdSet that produced it. Returning quietly here dropped \
             the coordinate, which resurfaced downstream as a missing \
             location and then as a (0, 0) way vertex, with no error \
             anywhere.",
            self.capacity_bytes
        );
        // SAFETY: byte_offset + 8 <= capacity_bytes = mmap length.
        // Pointer is 8-byte aligned (page-aligned base + 8*rank).
        // Atomic store eliminates data-race UB even with duplicate ids.
        unsafe {
            let ptr = self.base.add(byte_offset).cast::<AtomicU64>();
            (*ptr).store(packed, Ordering::Relaxed);
        }
        Ok(())
    }
}

/// Build the rank-indexed flat sparse array index from node blobs.
///
/// Steps:
///   1. `referenced.build_rank_index()` (~100 MB at planet).
///   2. Pre-allocate a `total * 8`-byte temp file via `set_len`,
///      mmap it `MmapMut`.
///   3. `parallel_scan_blobs_raw` over node blobs: workers extract
///      `(id, lat, lon)` tuples via `scan::node::extract_node_tuples`
///      (wire-only, no `PrimitiveBlock`). Each worker stores 8 bytes
///      into the mmap at `referenced.rank_if_set(id) << 3` for every
///      referenced id, via `AtomicU64::store(Relaxed)`. Workers do
///      not coordinate beyond the shared `&IdSet` and `SharedSparseWriter` -
///      each rank slot is touched at most once, atomic stores make
///      race-free even on duplicates.
///   4. Wrap the mmap into `SparseArrayIndex`, return.
///
/// `direct_io` is intentionally dropped: blob bodies are pread'd from
/// the shared input file handle on worker threads, incompatible with
/// `O_DIRECT` alignment requirements.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]
pub(super) fn build_node_index_sparse(
    input: &Path,
    _direct_io: bool,
    scratch_dir: &Path,
    mut referenced: IdSet,
) -> Result<SparseArrayIndex> {
    let t_rank = std::time::Instant::now();
    referenced.build_rank_index();
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    crate::debug::emit_counter(
        "altw_pass1_rank_index_ms",
        t_rank.elapsed().as_millis() as i64,
    );
    let total_bytes = referenced.total_count().saturating_mul(8);
    #[allow(clippy::cast_possible_wrap)]
    crate::debug::emit_counter("altw_pass1_store_bytes", total_bytes as i64);

    let file = open_anonymous_sparse_file(scratch_dir, total_bytes)?;

    // SAFETY: file is exclusively owned, opened read+write, sized to total_bytes.
    let mut mmap = unsafe {
        memmap2::MmapMut::map_mut(&file)
            .map_err(|e| format!("failed to mmap sparse index values: {e}"))?
    };

    let capacity_bytes = usize::try_from(total_bytes)
        .map_err(|_| "sparse index total_bytes does not fit in usize")?;
    let writer = SharedSparseWriter {
        base: mmap.as_mut_ptr(),
        capacity_bytes,
    };

    let (schedule, shared_input) = crate::scan::classify::build_classify_schedule(
        input,
        Some(crate::blob_meta::ElemKind::Node),
    )?;

    // Per-blob rank-span accounting: on sorted inputs each blob's
    // referenced nodes occupy one contiguous rank interval, so
    // sum(span) / stored is the write-amplification a per-blob
    // buffered-pwrite pass 1 would pay before it starts (the P5 gate
    // in notes/altw.md). Ratio near 1.0 = dense intervals; materially
    // above 1.0 = hole-heavy, buffered rewrite disqualified.
    let tuples_scanned = std::sync::atomic::AtomicU64::new(0);
    let coords_stored = std::sync::atomic::AtomicU64::new(0);
    let rank_span_slots = std::sync::atomic::AtomicU64::new(0);

    let referenced_ref = &referenced;
    let writer_ref = &writer;
    let tuples_scanned_ref = &tuples_scanned;
    let coords_stored_ref = &coords_stored;
    let rank_span_slots_ref = &rank_span_slots;
    type Scratch = (Vec<crate::scan::node::NodeTuple>, Vec<(usize, usize)>);
    crate::scan::classify::parallel_scan_blobs_raw(
        &shared_input,
        &schedule,
        None,
        || -> Scratch { (Vec::new(), Vec::new()) },
        |decompressed, (tuples, group_starts)| -> crate::error::Result<()> {
            tuples.clear();
            crate::scan::node::extract_node_tuples(decompressed, tuples, group_starts)?;
            let mut blob_stored: u64 = 0;
            let mut blob_min_rank: u64 = u64::MAX;
            let mut blob_max_rank: u64 = 0;
            for tup in tuples.iter() {
                if tup.id < 0 {
                    continue;
                }
                if let Some(rank) = referenced_ref.rank_if_set(tup.id) {
                    writer_ref.insert(tup.id, rank, tup.lat, tup.lon)?;
                    blob_stored += 1;
                    if rank < blob_min_rank {
                        blob_min_rank = rank;
                    }
                    if rank > blob_max_rank {
                        blob_max_rank = rank;
                    }
                }
            }
            tuples_scanned_ref.fetch_add(tuples.len() as u64, Ordering::Relaxed);
            if blob_stored > 0 {
                coords_stored_ref.fetch_add(blob_stored, Ordering::Relaxed);
                rank_span_slots_ref.fetch_add(blob_max_rank - blob_min_rank + 1, Ordering::Relaxed);
            }
            Ok(())
        },
        |_seq, ()| {},
    )?;

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter(
            "altw_pass1_tuples_scanned",
            tuples_scanned.load(Ordering::Relaxed) as i64,
        );
        crate::debug::emit_counter(
            "altw_pass1_coords_stored",
            coords_stored.load(Ordering::Relaxed) as i64,
        );
        crate::debug::emit_counter(
            "altw_pass1_rank_span_slots",
            rank_span_slots.load(Ordering::Relaxed) as i64,
        );
    }

    advise_random_store(&mmap);

    Ok(SparseArrayIndex {
        referenced,
        mmap,
        _file: file,
    })
}

/// Advise the kernel that subsequent access to the store is random.
///
/// Pass 2's lookups are genuinely random per ref (way refs scatter
/// uniformly over the id space), but the kernel's default readahead
/// speculates adjacent pages on every fault: europe measured ~37 KB of
/// disk read per major fault against a store where a lookup needs one
/// 4 KB page. MADV_RANDOM disables that speculation for the store.
/// Called after pass 1 completes so the near-sequential write pass
/// keeps default behaviour. Advisory only - correctness is unaffected
/// if the call fails - but the counter records whether it applied so a
/// flat measurement can be told apart from a probe that never fired.
/// Counter-signal on file: external's stage-4 coord mmap measured
/// MADV_RANDOM as a regression (killed useful readahead under 6
/// workers streaming semi-ordered payloads); sparse pass 2 has no such
/// ordering, which is exactly what this probe tests (notes/altw.md P3).
fn advise_random_store(mmap: &memmap2::MmapMut) {
    #[cfg(unix)]
    {
        let advised = mmap.advise(memmap2::Advice::Random).is_ok();
        crate::debug::emit_counter("altw_pass1_madvise_random", i64::from(advised));
    }
    #[cfg(not(unix))]
    let _ = mmap;
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{LAT_BIAS, PRESENT, pack, unpack};

    /// The slot encoding is asserted on its bits, not on a round trip
    /// through itself: a `pack` / `unpack` pair can agree with each other
    /// while both being wrong about the layout everything else depends on.
    #[test]
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap
    )]
    fn packed_layout_is_biased_latitude_presence_bit_and_raw_longitude() {
        let packed = pack(550_000_000, -120_000_000).unwrap();
        assert_ne!(packed & PRESENT, 0, "a written slot carries the flag bit");
        assert_eq!(
            (packed & 0x7fff_ffff) as i64 - LAT_BIAS,
            550_000_000,
            "the low word is the latitude biased by 90 degrees"
        );
        assert_eq!(
            (packed >> 32) as u32,
            (-120_000_000_i32) as u32,
            "the high word is the longitude verbatim"
        );
    }

    /// The whole point of the flag bit: Null Island is a real place, so a
    /// zero coordinate must decode as present while a zero *slot* - what
    /// `set_len` leaves behind - decodes as absent.
    #[test]
    fn null_island_is_present_and_an_untouched_slot_is_absent() {
        assert_eq!(unpack(0), None, "an untouched slot is absent");
        let packed = pack(0, 0).unwrap();
        assert_ne!(packed, 0, "a node at Null Island must not encode as zero");
        assert_eq!(unpack(packed), Some((0, 0)));
    }

    /// Every legal coordinate survives exactly, including both extremes of
    /// each axis, where the bias is tightest against the flag bit.
    #[test]
    fn every_legal_coordinate_survives_exactly() {
        for lat in [-900_000_000, -1, 0, 1, 900_000_000] {
            for lon in [-1_800_000_000, -1, 0, 1, 1_800_000_000] {
                let packed = pack(lat, lon).unwrap_or_else(|| {
                    panic!("({lat}, {lon}) is a legal coordinate and must pack")
                });
                assert_eq!(unpack(packed), Some((lat, lon)), "({lat}, {lon})");
            }
        }
    }

    /// Out-of-domain values are refused rather than stored as some other
    /// place. Latitude and longitude have different domains and are checked
    /// separately - a longitude of 100 degrees is fine, a latitude of 100
    /// degrees is not.
    #[test]
    fn out_of_domain_coordinates_are_refused() {
        assert!(pack(900_000_001, 0).is_none(), "latitude past 90 degrees");
        assert!(pack(-900_000_001, 0).is_none(), "latitude past -90 degrees");
        assert!(pack(0, 1_800_000_001).is_none(), "longitude past 180");
        assert!(pack(0, -1_800_000_001).is_none(), "longitude past -180");
        assert!(
            pack(1_000_000_000, 0).is_none(),
            "a latitude of 100 degrees is not a latitude, even though the \
             same number is a legal longitude"
        );
        assert!(pack(0, 1_000_000_000).is_some(), "100 degrees east is real");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sparse_store_precheck_reports_insufficient_space_before_mapping() {
        let dir = tempfile::tempdir().unwrap();
        let file = std::fs::File::create(dir.path().join("store")).unwrap();
        let err = super::preallocate_sparse_file(&file, dir.path(), u64::MAX)
            .expect_err("impossible allocation must be rejected");
        assert!(
            err.to_string()
                .contains("insufficient free space for sparse node index"),
            "wrong guard: {err}"
        );
        assert_eq!(file.metadata().unwrap().len(), 0);
    }

    /// The preallocation failure path used to `?` out between `create_new`
    /// and the unlink, leaving the PID-named store behind - after
    /// `posix_fallocate` may already have reserved its blocks. The guard has
    /// to hold from creation all the way through a successful unlink, so the
    /// assertion is on the scratch directory being empty, not on the error.
    #[cfg(target_os = "linux")]
    #[test]
    fn failed_preallocation_leaves_no_file_in_scratch() {
        let dir = tempfile::tempdir().unwrap();
        super::open_anonymous_sparse_file(dir.path(), u64::MAX)
            .expect_err("impossible allocation must be rejected");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert!(
            leftovers.is_empty(),
            "failed preallocation leaked scratch files: {leftovers:?}"
        );
    }

    /// And the success path must unlink too: the store is anonymous by
    /// design, held open only through the returned descriptor.
    #[test]
    fn successful_allocation_unlinks_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let file = super::open_anonymous_sparse_file(dir.path(), 4096).unwrap();
        assert_eq!(file.metadata().unwrap().len(), 4096);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
