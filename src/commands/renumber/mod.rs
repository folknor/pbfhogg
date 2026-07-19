//! External renumber for planet-scale input.
//!
//! The in-memory `renumber` module allocates three `FxHashMap<i64, i64>`
//! tables whose combined size on planet is ~278 GB, which OOM-kills any
//! host under ~300 GB RAM. This module uses `IdSet` bitsets with
//! rank-based O(1) lookup for all three element types - no hash maps,
//! no temp files, no mmaps.
//!
//! ## Architecture
//!
//! - **Pass 1**: parallel wire-format node rewriter (4 work-stealing
//!   workers). Each worker builds a per-shard `IdSet`; shards are
//!   merged after pass 1 and a rank index built for O(1) new-id lookup.
//! - **Stage 2d**: parallel wire-format way splice rewriter (6
//!   work-stealing workers). Resolves way refs inline via
//!   `node_id_set.rank()` during the splice - no intermediate files.
//!   Per-worker `IdSet` for `way_id_set`, merged after stage 2d.
//! - **R1**: sequential relation scan to collect all relation IDs into
//!   a third `IdSet` bitset + rank index.
//! - **R2d**: parallel wire-format splice rewriter for relations.
//!   Resolves node/way/relation member refs inline via `resolve()`.
//!
//! ## Orphan references
//!
//! Way refs and relation members whose old ID is not present in the
//! corresponding `IdSet` (i.e. not seen in the input) pass through
//! with their old ID unchanged, matching the in-memory path's
//! `unwrap_or(old_id)` behavior and osmium's semantics. Consumers that
//! assume new IDs are dense starting at `start_*_id` must tolerate
//! mixed old/new ID spaces in the output.
//!
//! Planet: 194 s (3m14s), 3.3 GB peak anon (commit `cb99106`).
//! Denmark cross-validated against in-memory mode on every commit.

use std::path::Path;

use super::{HeaderOverrides, Result, require_sorted, writer_from_header};
use crate::writer::Compression;

// ---------------------------------------------------------------------------
// Public types (moved from the former src/commands/renumber.rs)
// ---------------------------------------------------------------------------

/// Configuration for the renumber command.
pub struct RenumberOptions {
    pub start_node_id: i64,
    pub start_way_id: i64,
    pub start_relation_id: i64,
}

/// Statistics from a renumber operation.
#[derive(Debug, Clone)]
pub struct RenumberStats {
    pub nodes_written: u64,
    pub ways_written: u64,
    pub relations_written: u64,
    /// Way refs and relation members whose old ID was not found in the
    /// corresponding ID set. These pass through with their old ID
    /// unchanged (orphan passthrough).
    pub orphan_refs: u64,
}

impl RenumberStats {
    pub fn print_summary(&self) {
        let total = self.nodes_written + self.ways_written + self.relations_written;
        eprintln!(
            "Renumbered {total} elements: {} nodes, {} ways, {} relations",
            self.nodes_written, self.ways_written, self.relations_written,
        );
        if self.orphan_refs > 0 {
            eprintln!(
                "Warning: {} orphan refs preserved with old IDs (referenced \
                 elements not present in input)",
                self.orphan_refs,
            );
        }
    }
}

mod pass1;
mod relations;
mod schedule;
mod stage2;
mod wire_rewrite;

use pass1::pass1_parallel_scan;
use relations::{relation_r1_collect_ids, relation_r2d_assembly};
use schedule::build_all_blob_schedules;
use stage2::stage2d_parallel_way_assembly;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

pub(super) const PASS1_WORKERS: usize = 4;
/// Parallel way-assembly workers in stage 2d. Must be >= 1; the per-worker
/// `IdSet` merge at the R1 boundary calls `way_id_sets.remove(0)` which
/// panics on an empty vec.
pub(super) const STAGE2D_WORKERS: usize = 6;

// ---------------------------------------------------------------------------
// Shared instrumentation counters
// ---------------------------------------------------------------------------

/// Shared instrumentation counters for parallel worker stages.
/// All fields are AtomicU64 so workers can fetch_add concurrently.
/// Emit all counters via `emit()` after the scope joins workers.
pub(super) struct StageCounters {
    pub(super) pread_ms: std::sync::atomic::AtomicU64,
    pub(super) decompress_ms: std::sync::atomic::AtomicU64,
    pub(super) reframe_ms: std::sync::atomic::AtomicU64,
    pub(super) send_ms: std::sync::atomic::AtomicU64,
    pub(super) consumer_recv_ms: std::sync::atomic::AtomicU64,
    pub(super) consumer_write_ms: std::sync::atomic::AtomicU64,
    pub(super) blobs: std::sync::atomic::AtomicU64,
}

impl StageCounters {
    pub(super) fn new() -> Self {
        Self {
            pread_ms: std::sync::atomic::AtomicU64::new(0),
            decompress_ms: std::sync::atomic::AtomicU64::new(0),
            reframe_ms: std::sync::atomic::AtomicU64::new(0),
            send_ms: std::sync::atomic::AtomicU64::new(0),
            consumer_recv_ms: std::sync::atomic::AtomicU64::new(0),
            consumer_write_ms: std::sync::atomic::AtomicU64::new(0),
            blobs: std::sync::atomic::AtomicU64::new(0),
        }
    }

    #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
    pub(super) fn emit(&self, prefix: &str) {
        use std::sync::atomic::Ordering::Relaxed;
        crate::debug::emit_counter(
            &format!("{prefix}_pread_ms"),
            self.pread_ms.load(Relaxed) as i64,
        );
        crate::debug::emit_counter(
            &format!("{prefix}_decompress_ms"),
            self.decompress_ms.load(Relaxed) as i64,
        );
        crate::debug::emit_counter(
            &format!("{prefix}_reframe_ms"),
            self.reframe_ms.load(Relaxed) as i64,
        );
        crate::debug::emit_counter(
            &format!("{prefix}_send_ms"),
            self.send_ms.load(Relaxed) as i64,
        );
        crate::debug::emit_counter(
            &format!("{prefix}_consumer_recv_ms"),
            self.consumer_recv_ms.load(Relaxed) as i64,
        );
        crate::debug::emit_counter(
            &format!("{prefix}_consumer_write_ms"),
            self.consumer_write_ms.load(Relaxed) as i64,
        );
        crate::debug::emit_counter(&format!("{prefix}_blobs"), self.blobs.load(Relaxed) as i64);
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Run the planet-safe external renumber.
///
/// Four phases: pass 1 rewrites nodes (parallel wire-format rewriter),
/// stage 2d rewrites ways with resolved refs (parallel wire-format
/// splice), R1 collects relation IDs into IdSet, R2d rewrites
/// relations with resolved member refs (parallel wire-format splice).
/// All ID lookups are O(1) via `IdSet::resolve()`. No temp files.
#[allow(clippy::too_many_lines)]
#[hotpath::measure]
pub fn renumber_external(
    input: &Path,
    output: &Path,
    opts: &RenumberOptions,
    compression: Compression,
    direct_io: bool,
    overrides: &HeaderOverrides,
) -> Result<RenumberStats> {
    // Limit glibc malloc arenas to prevent cross-thread free
    // fragmentation. Without this, OwnedBlock Vec<u8>s allocated on
    // pass1/stage2d worker threads and freed on rayon compression
    // threads cause glibc arena accumulation growing to ~26 GB anon
    // RSS on planet. With 2 arenas the peak stays under 1 GB.
    // Scoped to this command - other pbfhogg commands are unaffected.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 2);
    }

    // ---- Header validation + output writer setup ----
    {
        let mut header_reader = crate::blob::BlobReader::open(input, direct_io)?;
        let header_blob = header_reader
            .next()
            .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;
        let header = header_blob.to_headerblock()?;
        require_sorted(&header, input, "Input PBF")?;
        super::warn_locations_on_ways_loss(&header);
    }
    // Re-parse header for writer construction (the earlier reader is dropped).
    let header = {
        let mut header_reader = crate::blob::BlobReader::open(input, direct_io)?;
        let header_blob = header_reader
            .next()
            .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;
        header_blob.to_headerblock()?
    };
    // Default to zlib:1 for external renumber - the compression pipeline
    // is on the critical path for pass 1 and stage 2d, and zlib:6 adds
    // ~22 s of backpressure at planet scale for ~15% smaller output.
    // Respect explicit caller overrides (e.g. --compression zlib:6).
    let effective_compression = if compression == Compression::default() {
        Compression::Zlib(1)
    } else {
        compression
    };
    let mut writer = writer_from_header(
        output,
        effective_compression,
        &header,
        true,
        overrides,
        #[allow(clippy::redundant_closure_for_method_calls)]
        |hb| hb.sorted(),
        direct_io,
        false,
    )?;
    // On any mid-stream error below (pass1 count mismatch, stage 2d
    // failure, relation rewrite failure, final flush) the partially
    // written output must not survive. Guard removes it on Drop unless
    // `commit()` is called at the end of the success path.
    let output_guard = crate::path_guard::PathGuard::file(output.to_path_buf());

    let mut stats = RenumberStats {
        nodes_written: 0,
        ways_written: 0,
        relations_written: 0,
        orphan_refs: 0,
    };

    // Single shared input fd for all phases - pread is concurrent-safe.
    let shared_file = std::sync::Arc::new(
        std::fs::File::open(input)
            .map_err(|e| format!("failed to open {}: {e}", input.display()))?,
    );

    crate::debug::emit_marker("RENUMBER_EXT_START");

    // ---- Blob schedule scan ----
    let t_sched = std::time::Instant::now();
    let (pass1_schedule, way_schedule, relation_schedule) = build_all_blob_schedules(input)?;
    #[allow(clippy::cast_possible_truncation)]
    crate::debug::emit_counter(
        "renumber_ext_schedule_ms",
        t_sched.elapsed().as_millis() as i64,
    );

    crate::debug::emit_marker("RENUMBER_EXT_PASS1_START");

    // ---- Pass 1: parallel node scan ----
    //
    // Work-stealing dispatch: workers claim blobs via AtomicUsize,
    // write into a shared IdSet via AtomicU8::fetch_or. Workers
    // pread → decompress → wire-format reframe (replace only ID deltas,
    // copy everything else verbatim) → send Vec<OwnedBlock> through a
    // bounded channel. Main thread reorders by seq and writes output.
    let pass1_total_nodes: u64 = pass1_schedule.iter().map(|t| t.element_count).sum();

    // Single shared IdSet - pre-allocate all chunks for the max
    // node ID so workers can use set_atomic(&self) concurrently.
    //
    // Scan the full schedule for max_id rather than trusting "last
    // blob's max_id == global max" (which holds only for a truthful
    // Sort.Type_then_ID header). A header that falsely advertises
    // sorted with content that is not would otherwise let a later
    // blob's id overshoot the pre-allocated bitset and panic in
    // `IdSet::set_atomic` with an opaque "pre_allocate only covers..."
    // message. O(N) in schedule length, called once at startup.
    let max_node_id = pass1_schedule.iter().map(|t| t.max_id).max().unwrap_or(0);
    let mut node_id_set = crate::idset::IdSet::new();
    node_id_set.pre_allocate(max_node_id);

    let nodes_written_atomic = std::sync::atomic::AtomicU64::new(0);

    pass1_parallel_scan(
        &pass1_schedule,
        opts.start_node_id,
        &shared_file,
        &node_id_set,
        &nodes_written_atomic,
        &mut writer,
    )?;

    stats.nodes_written += nodes_written_atomic.load(std::sync::atomic::Ordering::Relaxed);
    if stats.nodes_written != pass1_total_nodes {
        return Err(format!(
            "pass1 node count mismatch: schedule reported {pass1_total_nodes}, \
             workers wrote {}",
            stats.nodes_written,
        )
        .into());
    }

    crate::debug::emit_marker("RENUMBER_EXT_PASS1_END");

    // ---- Build rank index (no merge needed - single shared bitset) ----
    let t_rank = std::time::Instant::now();
    node_id_set.build_rank_index();
    #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
    {
        crate::debug::emit_counter(
            "renumber_ext_node_rank_ms",
            t_rank.elapsed().as_millis() as i64,
        );
        crate::debug::emit_counter(
            "renumber_ext_node_map_entries",
            node_id_set.total_count() as i64,
        );
    }

    // ---- Stage 2d: fused way resolve + rewrite (single pass) ----
    // Resolves way refs inline via node_id_set.rank() during
    // wire-format splice. No intermediate flat file or sidecar.
    crate::debug::emit_marker("RENUMBER_EXT_STAGE2D_START");
    let mut way_id_sets: Vec<crate::idset::IdSet> = (0..STAGE2D_WORKERS)
        .map(|_| crate::idset::IdSet::new())
        .collect();
    let stage2d_ways_atomic = std::sync::atomic::AtomicU64::new(0);
    let orphan_refs_atomic = std::sync::atomic::AtomicU64::new(0);
    stage2d_parallel_way_assembly(
        &shared_file,
        &mut writer,
        &mut way_id_sets,
        &way_schedule,
        &node_id_set,
        opts.start_node_id,
        opts.start_way_id,
        &stage2d_ways_atomic,
        &orphan_refs_atomic,
    )?;
    stats.ways_written += stage2d_ways_atomic.load(std::sync::atomic::Ordering::Relaxed);
    stats.orphan_refs += orphan_refs_atomic.load(std::sync::atomic::Ordering::Relaxed);
    crate::debug::emit_marker("RENUMBER_EXT_STAGE2D_END");

    // ---- R1: collect relation IDs into IdSet ----
    crate::debug::emit_marker("RENUMBER_EXT_R1_START");

    // Merge per-worker way_id_sets built during stage 2d.
    let t_way_merge = std::time::Instant::now();
    debug_assert!(
        STAGE2D_WORKERS >= 1 && !way_id_sets.is_empty(),
        "STAGE2D_WORKERS must be >= 1: way_id_sets.remove(0) panics on an empty vec"
    );
    let mut way_id_set = way_id_sets.remove(0);
    for other in way_id_sets {
        way_id_set.merge(other);
    }
    way_id_set.build_rank_index();
    #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
    {
        crate::debug::emit_counter(
            "renumber_ext_way_merge_rank_ms",
            t_way_merge.elapsed().as_millis() as i64,
        );
        crate::debug::emit_counter(
            "renumber_ext_way_map_entries",
            way_id_set.total_count() as i64,
        );
    }

    let mut relation_id_set = crate::idset::IdSet::new();
    relation_r1_collect_ids(&shared_file, &relation_schedule, &mut relation_id_set)?;
    let t_rel_rank = std::time::Instant::now();
    relation_id_set.build_rank_index();
    #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
    {
        crate::debug::emit_counter(
            "renumber_ext_rel_rank_ms",
            t_rel_rank.elapsed().as_millis() as i64,
        );
        crate::debug::emit_counter(
            "renumber_ext_relation_map_entries",
            relation_id_set.total_count() as i64,
        );
    }
    crate::debug::emit_marker("RENUMBER_EXT_R1_END");

    // ---- R2d: parallel wire-format rewrite of relations ----
    // Resolves node/way member refs inline via resolve().
    // No flat files, no mmaps, no sidecar.
    crate::debug::emit_marker("RENUMBER_EXT_R2D_START");
    relation_r2d_assembly(
        &shared_file,
        &relation_schedule,
        &mut writer,
        &node_id_set,
        opts.start_node_id,
        &way_id_set,
        opts.start_way_id,
        &relation_id_set,
        opts.start_relation_id,
        &mut stats,
    )?;
    crate::debug::emit_marker("RENUMBER_EXT_R2D_END");

    writer.flush()?;

    crate::debug::emit_marker("RENUMBER_EXT_END");

    // Output is fully written; release the guard so the file survives.
    output_guard.commit();

    Ok(stats)
}
