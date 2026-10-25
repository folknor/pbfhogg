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

use crate::counters::Stopwatch;

use super::{HeaderOverrides, Result, require_sorted, writer_from_header};
use crate::error::Error;
use crate::write::WriteOptions;
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
        Ok(())
    }

    /// Report, as a warning event, the orphan refs the run shifted. Silent
    /// when there are none. Separate from [`Self::write_summary`] because it
    /// is a warning, not part of the summary text.
    pub fn report_warnings(&self, diagnostics: &crate::host::Diagnostics) {
        if self.orphan_refs > 0 {
            crate::host::diag!(
                diagnostics,
                Warning,
                "renumber.orphan_refs",
                "{} orphan refs (referenced elements not present in \
                 input) shifted past their kind's renumbered range: new ID = \
                 old ID + start ID + element count, separately for nodes, \
                 ways and relations",
                self.orphan_refs,
            );
        }
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

/// The renumber phase a [`StageCounters`] set is emitted for. It selects the
/// emitted names only; it is not stored in the counters, so the set every
/// worker writes holds the seven atomics and nothing else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Pass1,
    Stage2d,
    R1,
    R2d,
}

/// Declares [`StageCounters`]: each counter once, with its emitted name for
/// every stage on the same line (columns: pass1, stage2d, r1, r2d), so every
/// sidecar name is greppable verbatim and a name cannot drift from its field.
/// `crate::counters::counter_set!` takes one name per counter, which would
/// need a type per stage; this is the same idea with a stage column.
macro_rules! stage_counters {
    (
        $(#[$sattr:meta])*
        struct $name:ident {
            $(
                $(#[$fattr:meta])*
                $field:ident => [$pass1:literal, $stage2d:literal, $r1:literal, $r2d:literal]
            ),* $(,)?
        }
    ) => {
        $(#[$sattr])*
        struct $name {
            $(
                $(#[$fattr])*
                $field: AtomicU64,
            )*
        }

        impl $name {
            const fn new() -> Self {
                Self {
                    $($field: AtomicU64::new(0),)*
                }
            }

            /// Visit every counter in declaration order with its name for
            /// `stage` and its current value.
            fn for_each(&self, stage: Phase, mut visit: impl FnMut(&'static str, u64)) {
                $(
                    visit(
                        match stage {
                            Phase::Pass1 => $pass1,
                            Phase::Stage2d => $stage2d,
                            Phase::R1 => $r1,
                            Phase::R2d => $r2d,
                        },
                        self.$field.load(Relaxed),
                    );
                )*
            }

            /// Emit every counter under its name for `stage`, in declaration
            /// order.
            fn emit(&self, stage: Phase) {
                self.for_each(stage, |name, value| crate::debug::emit_counter(name, value));
            }
        }
    };
}

stage_counters! {
    /// Per-stage timing counters, summed across workers and emitted once the
    /// stage ends. Timed in nanoseconds (`Stopwatch::record_ns`), not
    /// milliseconds: these counters sum one span per blob, and a planet node
    /// blob's pread is well under a millisecond, so per-blob `as_millis()`
    /// summed truncated zeros. The two `*_wait_ns` fields follow brokkr's stall
    /// convention (`brokkr sidecar <uuid> --stalls`): `window_wait` is workers
    /// parked because the writer side is a full window behind,
    /// `consumer_recv_wait` is the writer side idle for want of a finished blob.
    struct StageCounters {
        pread_ns => ["pass1_pread_ns", "stage2d_pread_ns", "r1_pread_ns", "r2d_pread_ns"],
        decompress_ns => [
            "pass1_decompress_ns",
            "stage2d_decompress_ns",
            "r1_decompress_ns",
            "r2d_decompress_ns"
        ],
        reframe_ns => ["pass1_reframe_ns", "stage2d_reframe_ns", "r1_reframe_ns", "r2d_reframe_ns"],
        consumer_write_ns => [
            "pass1_consumer_write_ns",
            "stage2d_consumer_write_ns",
            "r1_consumer_write_ns",
            "r2d_consumer_write_ns"
        ],
        window_wait_ns => [
            "pass1_window_wait_ns",
            "stage2d_window_wait_ns",
            "r1_window_wait_ns",
            "r2d_window_wait_ns"
        ],
        consumer_recv_wait_ns => [
            "pass1_consumer_recv_wait_ns",
            "stage2d_consumer_recv_wait_ns",
            "r1_consumer_recv_wait_ns",
            "r2d_consumer_recv_wait_ns"
        ],
        blobs => ["pass1_blobs", "stage2d_blobs", "r1_blobs", "r2d_blobs"],
    }
}

impl StageCounters {
    fn bump_blobs(&self) {
        self.blobs.fetch_add(1, Relaxed);
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
    ) -> crate::Result<()> {
        use std::os::unix::fs::FileExt as _;
        let t = crate::counters::Stopwatch::start();
        self.raw.resize(task.data_size, 0);
        file.read_exact_at(&mut self.raw, task.data_offset)
            .map_err(|e| {
                Error::from(e).context(format!("pread failed at offset {}", task.data_offset))
            })?;
        t.record_ns(&counters.pread_ns);

        let t = crate::counters::Stopwatch::start();
        crate::blob::decompress_blob_raw(&self.raw, &mut self.decompressed)?;
        t.record_ns(&counters.decompress_ns);
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
                return Err(Error::invalid_data(format!(
                    "renumber: {kind} start id {start} plus the input's {kind} count \
                     runs past the largest 64-bit id"
                )));
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
) -> crate::Result<crate::blob_meta::BlobIndex> {
    let max_id = count
        .checked_sub(1)
        .and_then(|span| i64::try_from(span).ok())
        .and_then(|span| base.checked_add(span))
        .ok_or_else(|| {
            Error::invalid_data(format!(
                "renumber: blob of {count} elements from id {base} overflows"
            ))
        })?;
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
            return Err(Error::invalid_argument(format!(
                "renumber: the {kind} start id must be at least 1, got {start}"
            )));
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
pub fn renumber_external(
    input: &Path,
    output: crate::output::Output,
    opts: &RenumberOptions,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<RenumberStats> {
    host.cancel.check()?;
    let result = renumber_external_to(input, &output, opts, write, overrides, host)?;
    output.publish(&host.cancel)?;
    Ok(result)
}

/// [`renumber_external`] writing to a borrowed output it does not publish.
#[allow(clippy::too_many_lines)]
#[hotpath::measure]
pub(crate) fn renumber_external_to(
    input: &Path,
    output: &crate::output::Output,
    opts: &RenumberOptions,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<RenumberStats> {
    let WriteOptions {
        compression,
        backend,
        direct_reads: direct_io,
        ..
    } = *write;
    validate_start_ids(opts)?;

    // OwnedBlock Vec<u8>s are allocated on pass1/stage2d worker threads
    // and freed on rayon compression threads. Uncapped, the arenas grow
    // to ~26 GB anon RSS on planet; with 2 the peak stays under 1 GB.
    host.process.cross_thread_alloc_pool();

    // ---- Header validation + output writer setup ----
    // The reader reports a file without a leading OsmHeader through the one
    // missing-header error, ahead of the sortedness check.
    let header = crate::ElementReader::open_in(input, direct_io, host)?
        .header()
        .clone();
    require_sorted(
        header.is_sorted(),
        input,
        "Input PBF",
        "Renumber assigns IDs by file position and resolves references by rank.",
    )?;
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
    // The writer fills the output's staging file; `renumber_external`
    // publishes it only after this function has succeeded.
    // Renumbered content has left the feed's ID space: the feed's diffs
    // address other objects than the ones in this file, so no replication
    // field is carried (osmium renumber writes a fresh header too).
    let mut writer = writer_from_header(
        output,
        effective_compression,
        &header,
        &super::output_header::OutputFacts {
            replication: super::output_header::Replication::Drop,
            ..super::output_header::OutputFacts::sorted(super::output_header::Ways::Reencoded)
        },
        overrides,
        backend,
        host,
    )?;

    // Single shared input fd for all phases - pread is concurrent-safe.
    let shared_file = crate::read::input::open(input)?;
    // The path every worker locates its errors against, captured once.
    let source: std::sync::Arc<Path> = std::sync::Arc::from(input);
    let file_len = shared_file
        .metadata()
        .map_err(|e| Error::from(e).context(format!("failed to stat {}", input.display())))?
        .len();

    crate::debug::emit_marker("RENUMBER_EXT_START");

    // ---- Blob schedule scan ----
    let t_sched = Stopwatch::start();
    let (pass1_schedule, way_schedule, relation_schedule) = build_all_blob_schedules(input, host)?;
    crate::debug::emit_counter("renumber_ext_schedule_ms", t_sched.elapsed_ms());

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
        &source,
        &node_id_set,
        &mut writer,
    )?;
    if pass1.nodes_written != pass1_total_nodes {
        return Err(Error::internal(format!(
            "pass1 node count mismatch: schedule reported {pass1_total_nodes}, \
             workers wrote {}",
            pass1.nodes_written,
        )));
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
    crate::debug::emit_counter("renumber_ext_node_overflow_ids", overflow_ids);
    crate::debug::emit_marker("RENUMBER_EXT_PASS1_END");

    // ---- Build rank index ----
    // Stage boundaries are checkpoints: the rank builds between the parallel
    // stages have none of their own.
    host.cancel.check()?;
    let t_rank = Stopwatch::start();
    let node_id_set = node_id_set.into_ranked();
    crate::debug::emit_counter("renumber_ext_node_rank_ms", t_rank.elapsed_ms());
    crate::debug::emit_counter("renumber_ext_node_map_entries", node_id_set.total_count());
    set_matches_written("node", node_id_set.total_count(), pass1.nodes_written)?;
    let nodes = IdSpace::new("node", &node_id_set, opts.start_node_id);

    // ---- Stage 2d: fused way resolve + rewrite (single pass) ----
    host.cancel.check()?;
    crate::debug::emit_marker("RENUMBER_EXT_STAGE2D_START");
    let mut way_id_sets: Vec<crate::idset::IdSet> = (0..RENUMBER_STAGE2D_WORKERS)
        .map(|_| crate::idset::IdSet::new())
        .collect();
    let stage2d = stage2d_parallel_way_assembly(
        &shared_file,
        &source,
        &mut writer,
        &mut way_id_sets,
        &way_schedule,
        &nodes,
        opts.start_way_id,
    )?;
    crate::debug::emit_marker("RENUMBER_EXT_STAGE2D_END");

    // ---- R1: collect relation IDs into IdSet ----
    crate::debug::emit_marker("RENUMBER_EXT_R1_START");
    let t_way_merge = Stopwatch::start();
    let mut way_id_set = crate::idset::IdSet::new();
    for other in way_id_sets {
        way_id_set.merge(other);
    }
    host.cancel.check()?;
    let way_id_set = way_id_set.into_ranked();
    crate::debug::emit_counter("renumber_ext_way_merge_rank_ms", t_way_merge.elapsed_ms());
    crate::debug::emit_counter("renumber_ext_way_map_entries", way_id_set.total_count());
    set_matches_written("way", way_id_set.total_count(), stage2d.ways_written)?;

    let mut relation_id_set = crate::idset::IdSet::new();
    relation_r1_collect_ids(
        &shared_file,
        &source,
        &relation_schedule,
        &mut relation_id_set,
        &host.cancel,
    )?;
    let t_rel_rank = Stopwatch::start();
    let relation_id_set = relation_id_set.into_ranked();
    crate::debug::emit_counter("renumber_ext_rel_rank_ms", t_rel_rank.elapsed_ms());
    let relation_count = relation_id_set.total_count();
    crate::debug::emit_counter("renumber_ext_relation_map_entries", relation_count);
    // Relation IDs are `start + rank`; the largest rank is count - 1.
    if relation_count > 0 {
        positional_last_fits(opts.start_relation_id, relation_count, "relation")?;
    }
    crate::debug::emit_marker("RENUMBER_EXT_R1_END");

    // ---- R2d: parallel wire-format rewrite of relations ----
    crate::debug::emit_marker("RENUMBER_EXT_R2D_START");
    let r2d = relation_r2d_assembly(
        &shared_file,
        &source,
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
        Err(Error::invalid_data(format!(
            "renumber: {kind} start id {start} plus the input's {count} {kind}s \
             runs past the largest 64-bit id"
        )))
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
        Err(Error::internal(format!(
            "renumber: collected {in_set} distinct {kind} ids but wrote {written} {kind}s"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The literal per-stage names equal the `{prefix}_{field}` names the
    /// counters were emitted under before they were spelled out, each paired
    /// with its own field's value.
    #[test]
    fn stage_counter_names_and_values_pair_up() {
        let counters = StageCounters::new();
        let fields: [(&str, &AtomicU64); 7] = [
            ("pread_ns", &counters.pread_ns),
            ("decompress_ns", &counters.decompress_ns),
            ("reframe_ns", &counters.reframe_ns),
            ("consumer_write_ns", &counters.consumer_write_ns),
            ("window_wait_ns", &counters.window_wait_ns),
            ("consumer_recv_wait_ns", &counters.consumer_recv_wait_ns),
            ("blobs", &counters.blobs),
        ];
        for (value, (_, field)) in (11_u64..).zip(fields) {
            field.store(value, Relaxed);
        }
        let stages = [
            (Phase::Pass1, "pass1"),
            (Phase::Stage2d, "stage2d"),
            (Phase::R1, "r1"),
            (Phase::R2d, "r2d"),
        ];
        for (stage, prefix) in stages {
            let mut seen = Vec::new();
            counters.for_each(stage, |name, value| seen.push((name.to_owned(), value)));
            let expected: Vec<(String, u64)> = (11_u64..)
                .zip(fields)
                .map(|(value, (field, _))| (format!("{prefix}_{field}"), value))
                .collect();
            assert_eq!(seen, expected, "{stage:?}");
        }
    }

    fn task(count: u64) -> BlobTask {
        BlobTask {
            blob_index: 0,
            frame_offset: 0,
            data_offset: 0,
            data_size: 0,
            element_count: count,
            max_id: 0,
            bbox: None,
        }
    }

    #[derive(Default)]
    struct Collect(std::sync::Mutex<Vec<(crate::host::EventKind, &'static str, String)>>);
    impl crate::host::DiagnosticSink for Collect {
        fn event(&self, e: &crate::host::Event<'_>) {
            self.0
                .lock()
                .expect("sink")
                .push((e.kind, e.key, e.args.to_string()));
        }
    }

    #[test]
    fn orphan_refs_are_a_warning_event_not_summary_text() {
        let sink = std::sync::Arc::new(Collect::default());
        let diagnostics = crate::host::Diagnostics::new(
            std::sync::Arc::clone(&sink) as std::sync::Arc<dyn crate::host::DiagnosticSink>
        );
        let mut stats = RenumberStats {
            nodes_written: 1,
            ways_written: 0,
            relations_written: 0,
            orphan_refs: 0,
        };
        stats.report_warnings(&diagnostics);
        assert!(sink.0.lock().expect("sink").is_empty());

        stats.orphan_refs = 3;
        let mut summary = Vec::new();
        stats.write_summary(&mut summary).expect("summary");
        let summary = String::from_utf8(summary).expect("utf-8");
        assert!(!summary.contains("orphan"), "summary: {summary}");
        stats.report_warnings(&diagnostics);
        let events = sink.0.lock().expect("sink");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, crate::host::EventKind::Warning);
        assert_eq!(events[0].1, "renumber.orphan_refs");
        assert!(events[0].2.starts_with("3 orphan refs"), "{}", events[0].2);
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
