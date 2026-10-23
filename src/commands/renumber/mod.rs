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
//! - **Pass 1**: parallel wire-format node rewriter. Every worker records
//!   old node IDs in one shared, pre-allocated `IdSet`; a rank index built
//!   after the pass gives O(1) old-to-new lookup.
//! - **Stage 2d**: parallel wire-format way splice rewriter. Resolves way
//!   refs inline through the node set's rank during the splice - no
//!   intermediate files. Per-worker `IdSet` shards for `way_id_set`, merged
//!   after stage 2d.
//! - **R1**: sequential relation scan to collect all relation IDs into
//!   a third `IdSet` + rank index.
//! - **R2d**: parallel wire-format splice rewriter for relations.
//!   Resolves node/way/relation member refs inline.
//!
//! The three parallel stages run on one ordered pool (`pool.rs`), which ends
//! the run promptly on any error and bounds the results in flight.
//!
//! ## Position and rank must agree
//!
//! An element's new ID is assigned by *position* (start plus the number of
//! elements of its kind before it); a reference's new ID is computed by
//! *rank* (start plus the number of input IDs below the referenced one).
//! They name the same element only when input IDs strictly increase within
//! each kind, so renumber verifies exactly that - per element inside every
//! rewriter, and across blobs in schedule order - and refuses input that
//! fails it, whatever its header claims about sorting.
//!
//! ## Orphan references
//!
//! A way ref or relation member whose target is not in the input (an
//! orphan) with old ID X becomes `start + count + X`, where `start` and
//! `count` are that kind's start ID and number of input elements: the
//! first ID past the renumbered range, plus the old ID. Passing it through
//! unchanged let X equal the new ID of a different element. The shift needs
//! no extra pass, keeps two refs to one orphan equal, keeps X recoverable by
//! subtraction, and refuses a shift past `i64::MAX`
//! (`wire_rewrite::IdSpace`). The rule is per kind, so it only holds when
//! each kind's count equals its set's cardinality, which
//! [`renumber_external`] checks after every stage.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

use super::{HeaderOverrides, Result, require_sorted, writer_from_header};
use crate::writer::Compression;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Configuration for the renumber command. Every start ID must be at least
/// 1: a negative or zero start would hand out ID 0 or negative IDs, which
/// the rest of the crate rejects as input.
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
    /// Way refs and relation members whose target is not in the input.
    /// Each became old ID + start ID + input element count of its kind,
    /// past that kind's renumbered range.
    pub orphan_refs: u64,
}

impl RenumberStats {
    pub fn write_summary(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        let total = self
            .nodes_written
            .saturating_add(self.ways_written)
            .saturating_add(self.relations_written);
        writeln!(
            w,
            "Renumbered {total} elements: {} nodes, {} ways, {} relations",
            self.nodes_written, self.ways_written, self.relations_written,
        )?;
        if self.orphan_refs > 0 {
            writeln!(
                w,
                "Warning: {} orphan refs (referenced elements not present in \
                 input) shifted past their kind's renumbered range: new ID = \
                 old ID + start ID + element count, separately for nodes, \
                 ways and relations",
                self.orphan_refs,
            )?;
        }

        Ok(())
    }
}

mod pass1;
mod pool;
mod relations;
mod schedule;
mod stage2;
mod wire_rewrite;

use pass1::pass1_parallel_scan;
use relations::{relation_r1_collect_ids, relation_r2d_assembly};
use schedule::{BlobTask, build_all_blob_schedules};
use stage2::stage2d_parallel_way_assembly;
use wire_rewrite::IdSpace;

use crate::tunables::{RENUMBER_PREALLOC_IDS_PER_BYTE, RENUMBER_STAGE2D_WORKERS};

// ---------------------------------------------------------------------------
// Shared instrumentation counters
// ---------------------------------------------------------------------------

/// Add the nanoseconds since `since` to `counter`.
///
/// Nanoseconds, not milliseconds: these counters sum one span per blob, and
/// a planet node blob's pread is well under a millisecond, so per-blob
/// `as_millis()` summed truncated zeros.
fn add_ns(counter: &AtomicU64, since: Instant) {
    let ns = u64::try_from(since.elapsed().as_nanos()).unwrap_or(u64::MAX);
    counter.fetch_add(ns, Relaxed);
}

/// Per-stage timing counters, summed across workers and emitted once the
/// stage ends. The two `*_wait_ns` fields follow brokkr's stall convention
/// (`brokkr sidecar <uuid> --stalls`): `window_wait` is workers parked
/// because the writer side is a full window behind, `consumer_recv_wait` is
/// the writer side idle for want of a finished blob.
struct StageCounters {
    pread_ns: AtomicU64,
    decompress_ns: AtomicU64,
    reframe_ns: AtomicU64,
    consumer_write_ns: AtomicU64,
    window_wait_ns: AtomicU64,
    consumer_recv_wait_ns: AtomicU64,
    blobs: AtomicU64,
}

impl StageCounters {
    fn new() -> Self {
        Self {
            pread_ns: AtomicU64::new(0),
            decompress_ns: AtomicU64::new(0),
            reframe_ns: AtomicU64::new(0),
            consumer_write_ns: AtomicU64::new(0),
            window_wait_ns: AtomicU64::new(0),
            consumer_recv_wait_ns: AtomicU64::new(0),
            blobs: AtomicU64::new(0),
        }
    }

    fn bump_blobs(&self) {
        self.blobs.fetch_add(1, Relaxed);
    }

    fn emit(&self, prefix: &str) {
        let fields: [(&str, &AtomicU64); 7] = [
            ("pread_ns", &self.pread_ns),
            ("decompress_ns", &self.decompress_ns),
            ("reframe_ns", &self.reframe_ns),
            ("consumer_write_ns", &self.consumer_write_ns),
            ("window_wait_ns", &self.window_wait_ns),
            ("consumer_recv_wait_ns", &self.consumer_recv_wait_ns),
            ("blobs", &self.blobs),
        ];
        for (name, value) in fields {
            let v = i64::try_from(value.load(Relaxed)).unwrap_or(i64::MAX);
            crate::debug::emit_counter(&format!("{prefix}_{name}"), v);
        }
    }
}

// ---------------------------------------------------------------------------
// Shared per-blob helpers
// ---------------------------------------------------------------------------

/// A worker's read and decompress buffers.
#[derive(Default)]
struct BlobBuffers {
    raw: Vec<u8>,
    decompressed: Vec<u8>,
}

impl BlobBuffers {
    // Deliberately read only the payload, never the tagdata header or key lists.
    // Renumber is a terminal step whose output is not tag-filtered; adding header
    // reads or key parsing adds work to the measured per-blob rewrite hot path.
    /// pread the blob `task` addresses and decompress it into
    /// `self.decompressed`.
    fn load(
        &mut self,
        file: &std::fs::File,
        task: &BlobTask,
        counters: &StageCounters,
    ) -> std::result::Result<(), String> {
        use std::os::unix::fs::FileExt as _;
        let t = Instant::now();
        self.raw.resize(task.data_size, 0);
        file.read_exact_at(&mut self.raw, task.data_offset)
            .map_err(|e| format!("pread failed at offset {}: {e}", task.data_offset))?;
        add_ns(&counters.pread_ns, t);

        let t = Instant::now();
        crate::blob::decompress_blob_raw(&self.raw, &mut self.decompressed)
            .map_err(|e| e.to_string())?;
        add_ns(&counters.decompress_ns, t);
        Ok(())
    }
}

/// Per-blob base new IDs for a positionally numbered kind: blob `i` starts
/// at `start` plus the declared element counts of blobs `0..i`.
///
/// The counts are indexdata claims, verified per blob after decode. What is
/// checked here is that the *last* ID handed out, `start + total - 1`, fits
/// an `i64`; the one-past-the-end cursor never has to. An empty blob's base
/// is never used for an ID (an empty blob is not written, and a decoded
/// count that disagrees with its claim of zero is an error), so it may
/// saturate.
fn positional_bases(start: i64, schedule: &[BlobTask], kind: &str) -> Result<Vec<i64>> {
    let mut bases = Vec::with_capacity(schedule.len());
    let mut cursor = i128::from(start);
    for task in schedule {
        let count = i128::from(task.element_count);
        if count > 0 {
            let last = cursor
                .checked_add(count)
                .and_then(|end| end.checked_sub(1))
                .and_then(|last| i64::try_from(last).ok());
            if last.is_none() {
                return Err(format!(
                    "renumber: {kind} start id {start} plus the input's {kind} count \
                     runs past the largest 64-bit id"
                )
                .into());
            }
        }
        bases.push(i64::try_from(cursor).unwrap_or(i64::MAX));
        cursor = cursor.saturating_add(count);
    }
    Ok(bases)
}

/// The output `BlobIndex` for a non-empty, positionally numbered blob.
fn blob_index(
    kind: crate::ElementKind,
    base: i64,
    count: u64,
    bbox: Option<crate::blob_meta::BlobBbox>,
) -> std::result::Result<crate::blob_meta::BlobIndex, String> {
    let max_id = count
        .checked_sub(1)
        .and_then(|span| i64::try_from(span).ok())
        .and_then(|span| base.checked_add(span))
        .ok_or_else(|| format!("renumber: blob of {count} elements from id {base} overflows"))?;
    Ok(crate::blob_meta::BlobIndex {
        kind,
        min_id: base,
        max_id,
        count,
        bbox,
    })
}

/// Refuse a start ID below 1. A start of 0 hands out ID 0, a negative one
/// hands out negative IDs and then 0 - IDs this crate refuses as input
/// everywhere (DEVIATIONS.md, "Negative input IDs rejected project-wide").
/// osmium counts *downwards* from a negative start instead; renumber does not
/// offer that numbering.
fn validate_start_ids(opts: &RenumberOptions) -> Result<()> {
    for (kind, start) in [
        ("node", opts.start_node_id),
        ("way", opts.start_way_id),
        ("relation", opts.start_relation_id),
    ] {
        if start < 1 {
            return Err(
                format!("renumber: the {kind} start id must be at least 1, got {start}").into(),
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Run the planet-safe external renumber.
///
/// Four phases: pass 1 rewrites nodes (parallel wire-format rewriter),
/// stage 2d rewrites ways with resolved refs (parallel wire-format
/// splice), R1 collects relation IDs into an IdSet, R2d rewrites
/// relations with resolved member refs (parallel wire-format splice).
/// No temp files.
///
/// Refuses input whose node, way or relation IDs do not strictly increase
/// in file order (see the module docs), and start IDs below 1.
#[allow(clippy::too_many_lines)]
#[hotpath::measure]
pub fn renumber_external(
    input: &Path,
    output: &Path,
    opts: &RenumberOptions,
    compression: Compression,
    direct_io: bool,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<RenumberStats> {
    validate_start_ids(opts)?;

    // OwnedBlock Vec<u8>s are allocated on pass1/stage2d worker threads
    // and freed on rayon compression threads. Uncapped, the arenas grow
    // to ~26 GB anon RSS on planet; with 2 the peak stays under 1 GB.
    host.process.cross_thread_alloc_pool();

    // ---- Header validation + output writer setup ----
    let header = {
        let mut header_reader = crate::blob::BlobReader::open_in(input, direct_io, host)?;
        let header_blob = header_reader
            .next()
            .ok_or_else(|| crate::error::new_error(crate::error::ErrorKind::MissingHeader))??;
        header_blob.to_headerblock()?
    };
    require_sorted(
        header.is_sorted(),
        input,
        "Input PBF",
        "Renumber assigns IDs by file position and resolves references by rank.",
    )?;
    super::warn_locations_on_ways_loss(&header, host);
    // Preserve the external-renumber zlib:1 default documented in
    // reference/performance.md. This legacy value-based API cannot distinguish
    // omitted compression from an explicit Zlib(6): both arrive as the same
    // Compression value and both select level 1 here. Other values pass through.
    // Removing this branch would change the default on the measured write path.
    // Honoring explicit level 6 while retaining the default requires carrying
    // omission separately (for example Option<Compression>) from CLI parsing
    // through this entry point and migrating its library callers together.
    let effective_compression = if compression == Compression::default() {
        Compression::Zlib(1)
    } else {
        compression
    };
    // `output` is written in place. Protecting a *user-named* destination
    // from a mid-run failure is the front end's job, not this function's:
    // only the caller knows whether the path it passed is a name a user
    // chose or a scratch file the run created. `pbfhogg-cli` routes every
    // `--output` through `path_guard::OutputFile` for exactly that reason,
    // so this entry point is already handed a scratch sibling.
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
        host,
    )?;

    // Single shared input fd for all phases - pread is concurrent-safe.
    let shared_file = std::fs::File::open(input)
        .map_err(|e| format!("failed to open {}: {e}", input.display()))?;
    let file_len = shared_file
        .metadata()
        .map_err(|e| format!("failed to stat {}: {e}", input.display()))?
        .len();

    crate::debug::emit_marker("RENUMBER_EXT_START");

    // ---- Blob schedule scan ----
    let t_sched = Instant::now();
    let (pass1_schedule, way_schedule, relation_schedule) = build_all_blob_schedules(input, host)?;
    emit_elapsed_ms("renumber_ext_schedule_ms", t_sched);

    // ---- Pass 1: parallel node rewrite ----
    crate::debug::emit_marker("RENUMBER_EXT_PASS1_START");
    let pass1_total_nodes: u64 = pass1_schedule
        .iter()
        .fold(0_u64, |acc, t| acc.saturating_add(t.element_count));

    // One shared IdSet, pre-allocated so workers can `set_atomic_checked`
    // through `&self`. The whole schedule is scanned for the claimed max
    // rather than trusting the last blob; the claim is then bounded by the
    // file size (see `RENUMBER_PREALLOC_IDS_PER_BYTE`).
    let mut node_id_set = crate::idset::IdSet::new();
    if let Some(claimed_max) = pass1_schedule.iter().map(|t| t.max_id).max() {
        let bound = crate::idset::IdSet::file_bounded_max_id(
            claimed_max,
            file_len,
            RENUMBER_PREALLOC_IDS_PER_BYTE,
        );
        if bound >= 0 {
            node_id_set.pre_allocate(bound);
        }
    }

    let pass1 = pass1_parallel_scan(
        &pass1_schedule,
        opts.start_node_id,
        &shared_file,
        &node_id_set,
        &mut writer,
    )?;
    if pass1.nodes_written != pass1_total_nodes {
        return Err(format!(
            "pass1 node count mismatch: schedule reported {pass1_total_nodes}, \
             workers wrote {}",
            pass1.nodes_written,
        )
        .into());
    }
    // IDs the pre-allocation did not cover, inserted now that no worker
    // holds the set. Already checked non-negative and strictly increasing.
    let mut overflow_ids: u64 = 0;
    for ids in pass1.overflow {
        overflow_ids = overflow_ids.saturating_add(u64::try_from(ids.len()).unwrap_or(u64::MAX));
        for id in ids {
            node_id_set.set(id);
        }
    }
    crate::debug::emit_counter(
        "renumber_ext_node_overflow_ids",
        i64::try_from(overflow_ids).unwrap_or(i64::MAX),
    );
    crate::debug::emit_marker("RENUMBER_EXT_PASS1_END");

    // ---- Build rank index ----
    let t_rank = Instant::now();
    node_id_set.build_rank_index();
    emit_elapsed_ms("renumber_ext_node_rank_ms", t_rank);
    emit_count("renumber_ext_node_map_entries", node_id_set.total_count());
    set_matches_written("node", node_id_set.total_count(), pass1.nodes_written)?;
    let nodes = IdSpace::new("node", &node_id_set, opts.start_node_id);

    // ---- Stage 2d: fused way resolve + rewrite (single pass) ----
    crate::debug::emit_marker("RENUMBER_EXT_STAGE2D_START");
    let mut way_id_sets: Vec<crate::idset::IdSet> = (0..RENUMBER_STAGE2D_WORKERS)
        .map(|_| crate::idset::IdSet::new())
        .collect();
    let stage2d = stage2d_parallel_way_assembly(
        &shared_file,
        &mut writer,
        &mut way_id_sets,
        &way_schedule,
        &nodes,
        opts.start_way_id,
    )?;
    crate::debug::emit_marker("RENUMBER_EXT_STAGE2D_END");

    // ---- R1: collect relation IDs into IdSet ----
    crate::debug::emit_marker("RENUMBER_EXT_R1_START");
    let t_way_merge = Instant::now();
    let mut way_id_set = crate::idset::IdSet::new();
    for other in way_id_sets {
        way_id_set.merge(other);
    }
    way_id_set.build_rank_index();
    emit_elapsed_ms("renumber_ext_way_merge_rank_ms", t_way_merge);
    emit_count("renumber_ext_way_map_entries", way_id_set.total_count());
    set_matches_written("way", way_id_set.total_count(), stage2d.ways_written)?;

    let mut relation_id_set = crate::idset::IdSet::new();
    relation_r1_collect_ids(&shared_file, &relation_schedule, &mut relation_id_set)?;
    let t_rel_rank = Instant::now();
    relation_id_set.build_rank_index();
    emit_elapsed_ms("renumber_ext_rel_rank_ms", t_rel_rank);
    let relation_count = relation_id_set.total_count();
    emit_count("renumber_ext_relation_map_entries", relation_count);
    // Relation IDs are `start + rank`; the largest rank is count - 1.
    if relation_count > 0 {
        positional_last_fits(opts.start_relation_id, relation_count, "relation")?;
    }
    crate::debug::emit_marker("RENUMBER_EXT_R1_END");

    // ---- R2d: parallel wire-format rewrite of relations ----
    crate::debug::emit_marker("RENUMBER_EXT_R2D_START");
    let r2d = relation_r2d_assembly(
        &shared_file,
        &relation_schedule,
        &mut writer,
        &wire_rewrite::RelationIdSpaces {
            relations: IdSpace::new("relation", &relation_id_set, opts.start_relation_id),
            nodes,
            ways: IdSpace::new("way", &way_id_set, opts.start_way_id),
        },
        host,
    )?;
    // Relations are numbered by rank, not position, so this is not about
    // numbering: a relation R1 collected but R2d did not write would leave
    // refs to it resolving as live while it is absent from the output.
    set_matches_written("relation", relation_count, r2d.relations_written)?;
    crate::debug::emit_marker("RENUMBER_EXT_R2D_END");

    writer.flush()?;

    crate::debug::emit_marker("RENUMBER_EXT_END");

    Ok(RenumberStats {
        nodes_written: pass1.nodes_written,
        ways_written: stage2d.ways_written,
        relations_written: r2d.relations_written,
        orphan_refs: stage2d.orphan_refs.saturating_add(r2d.orphan_refs),
    })
}

fn positional_last_fits(start: i64, count: u64, kind: &str) -> Result<()> {
    let fits = count
        .checked_sub(1)
        .and_then(|span| i64::try_from(span).ok())
        .and_then(|span| start.checked_add(span))
        .is_some();
    if fits {
        Ok(())
    } else {
        Err(format!(
            "renumber: {kind} start id {start} plus the input's {count} {kind}s \
             runs past the largest 64-bit id"
        )
        .into())
    }
}

/// Refuse a run whose ID set and written count for `kind` disagree. A
/// reference resolves by rank in the set while an element is numbered by
/// position, and an orphan is shifted to `start + set cardinality`: if the
/// two counts differed, a live ref could name the wrong element and an
/// orphan could land inside the renumbered range. The order checks already
/// make them equal; this states the invariant the orphan shift rests on.
fn set_matches_written(kind: &str, in_set: u64, written: u64) -> Result<()> {
    if in_set == written {
        Ok(())
    } else {
        Err(
            format!("renumber: collected {in_set} distinct {kind} ids but wrote {written} {kind}s")
                .into(),
        )
    }
}

fn emit_elapsed_ms(name: &str, since: Instant) {
    crate::debug::emit_counter(
        name,
        i64::try_from(since.elapsed().as_millis()).unwrap_or(i64::MAX),
    );
}

fn emit_count(name: &str, count: u64) {
    crate::debug::emit_counter(name, i64::try_from(count).unwrap_or(i64::MAX));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(count: u64) -> BlobTask {
        BlobTask {
            data_offset: 0,
            data_size: 0,
            element_count: count,
            max_id: 0,
            bbox: None,
        }
    }

    #[test]
    fn bases_are_a_prefix_sum_and_only_the_last_id_must_fit() {
        let bases = positional_bases(10, &[task(3), task(0), task(2)], "node").expect("fits");
        assert_eq!(bases, vec![10, 13, 13]);
        // One node starting at i64::MAX: the only id handed out fits, though
        // the one-past-the-end cursor does not. Trailing empty blob too.
        assert!(positional_bases(i64::MAX, &[task(1), task(0)], "node").is_ok());
        assert!(positional_bases(i64::MAX, &[task(2)], "node").is_err());
    }

    #[test]
    fn start_ids_below_one_are_refused() {
        for (n, w, r) in [(0, 1, 1), (1, -5, 1), (1, 1, 0)] {
            let opts = RenumberOptions {
                start_node_id: n,
                start_way_id: w,
                start_relation_id: r,
            };
            assert!(validate_start_ids(&opts).is_err(), "{n},{w},{r}");
        }
        let ok = RenumberOptions {
            start_node_id: 1,
            start_way_id: 5000,
            start_relation_id: 1,
        };
        assert!(validate_start_ids(&ok).is_ok());
    }

    /// An indexdata claim of `i64::MAX` from a small file must not
    /// size the pre-allocation; an honest planet-scale claim still does.
    #[test]
    fn the_node_preallocation_is_bounded_by_the_file() {
        let bound = |claimed, file_len| {
            crate::idset::IdSet::file_bounded_max_id(
                claimed,
                file_len,
                RENUMBER_PREALLOC_IDS_PER_BYTE,
            )
        };
        assert_eq!(bound(i64::MAX, 4096), 262_144);
        let planet = 80_u64 * 1024 * 1024 * 1024;
        assert_eq!(bound(13_000_000_000, planet), 13_000_000_000);
        assert_eq!(bound(i64::MAX, u64::MAX), i64::MAX);
    }
}
