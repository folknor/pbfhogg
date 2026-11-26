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
//!   intermediate files. Old way IDs go into one shared, pre-allocated
//!   `way_id_set`, as pass 1 does for nodes.
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
//! An element's new ID is assigned by *position* (the number of elements of
//! its kind before it, counted up from a positive start or down from a
//! negative one); a reference's new ID is computed by *rank* (the number of
//! input IDs canonically before the referenced one, counted the same way).
//! They name the same element only when input IDs strictly ascend in
//! canonical order (`0`, then `-1, -2, ...`, then `1, 2, ...`) within each
//! kind, as osmium requires, so renumber verifies exactly that - per element
//! inside every rewriter, and across blobs in schedule order - and refuses
//! input that fails it, whatever its header claims about sorting. Negative
//! input IDs are renumbered like any other.
//!
//! ## Orphan references
//!
//! A way ref or relation member whose target is not in the input (an
//! orphan) is named by a per-kind bijection of the absent IDs onto the IDs
//! outside the renumbered range, built from the rank index alone
//! (`wire_rewrite::IdSpace`): no extra pass, two refs to one orphan stay
//! equal, no orphan names a renumbered element, and no orphan can overflow.
//! Passing orphans through unchanged let one equal the new ID of a
//! different element. The rule is per kind, so it only holds when each
//! kind's count equals its set's cardinality, which [`renumber_external`]
//! checks after every stage.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use crate::counters::Stopwatch;

use super::{HeaderOverrides, Result, require_sorted, writer_from_header};
use crate::error::Error;
use crate::read::input::{InputSource, PinnedInput};
use crate::write::WriteOptions;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Configuration for the renumber command. Each kind counts up from a
/// positive start and down from a negative one, as osmium does; a start of
/// 0 means 1, as in osmium. Only the last ID handed out must fit an `i64`.
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
    /// Each is named outside its kind's renumbered range, one name per
    /// distinct absent ID.
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

    /// Report, as a warning event, the orphan refs the run named. Silent
    /// when there are none. Separate from [`Self::write_summary`] because it
    /// is a warning, not part of the summary text.
    pub fn report_warnings(&self, diagnostics: &crate::host::Diagnostics) {
        if self.orphan_refs > 0 {
            crate::host::diag!(
                diagnostics,
                Warning,
                "renumber.orphan_refs",
                "{} orphan refs (referenced elements not present in \
                 input) named outside their kind's renumbered range, one new \
                 ID per absent ID, in the absent IDs' order, separately for \
                 nodes, ways and relations",
                self.orphan_refs,
            );
        }
    }
}

mod pass1;
mod pool;
mod relations;
mod schedule;
mod split;
mod stage2;
mod wire_rewrite;

use pass1::pass1_parallel_scan;
use relations::{relation_r1_collect_ids, relation_r2d_assembly};
use schedule::{BlobTask, build_all_blob_schedules};
use stage2::stage2d_parallel_way_assembly;
use wire_rewrite::IdSpace;

use crate::tunables::RENUMBER_PREALLOC_IDS_PER_BYTE;

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
            fn emit(&self, stage: Phase, invocation: &crate::host::Recorder) {
                self.for_each(stage, |name, value| invocation.counter(name, value));
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
    raw: crate::read::read_buf::ReadBuf,
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
        file: &crate::read::input::InputFile,
        task: &BlobTask,
        counters: &StageCounters,
    ) -> crate::Result<()> {
        let t = crate::counters::Stopwatch::start();
        let raw = self
            .raw
            .read_input_at(file, task.data_offset, task.data_size)
            .map_err(|e| {
                Error::from(e).context(format!("pread failed at offset {}", task.data_offset))
            })?;
        t.record_ns(&counters.pread_ns);

        let t = crate::counters::Stopwatch::start();
        crate::blob::decompress_blob_raw(raw, &mut self.decompressed)?;
        t.record_ns(&counters.decompress_ns);
        Ok(())
    }

    /// Apply the retention limits once a blob is done with both buffers: the
    /// read buffer's (read/read_buf.rs) and the decode pool's for the decode
    /// buffer (read/decompress.rs, "Retention policy").
    fn release_outliers(&mut self, invocation: &crate::host::Recorder) {
        self.raw.release_outlier(invocation);
        crate::blob::release_decode_outlier(&mut self.decompressed, invocation);
    }
}

/// A rewrite worker's state, whose blob buffers the rewrite pool releases
/// after every blob.
trait RewriteWorker {
    fn buffers(&mut self) -> &mut BlobBuffers;
}

/// Per-blob base new IDs for a positionally numbered kind: blob `i` starts
/// at `start` moved by the declared element counts of blobs `0..i`, up from
/// a positive start, down from a negative one.
///
/// The counts are indexdata claims, verified per blob after decode. What is
/// checked here is that the *last* ID handed out fits an `i64`; the
/// one-past-the-end cursor never has to. An empty blob's base is never used
/// for an ID (an empty blob is not written, and a decoded count that
/// disagrees with its claim of zero is an error), so it may saturate.
fn positional_bases(start: i64, schedule: &[BlobTask], kind: &str) -> Result<Vec<i64>> {
    let mut bases = Vec::with_capacity(schedule.len());
    let up = start > 0;
    let mut cursor = i128::from(start);
    for task in schedule {
        let count = i128::from(task.element_count);
        if count > 0 {
            let last = if up {
                cursor.checked_add(count).and_then(|end| end.checked_sub(1))
            } else {
                cursor.checked_sub(count).and_then(|end| end.checked_add(1))
            }
            .and_then(|last| i64::try_from(last).ok());
            if last.is_none() {
                return Err(Error::invalid_data(format!(
                    "renumber: {kind} start id {start} plus the input's {kind} count \
                     runs past the 64-bit id range"
                )));
            }
        }
        let saturated = if up { i64::MAX } else { i64::MIN };
        bases.push(i64::try_from(cursor).unwrap_or(saturated));
        cursor = if up {
            cursor.saturating_add(count)
        } else {
            cursor.saturating_sub(count)
        };
    }
    Ok(bases)
}

/// The output `BlobIndex` for a non-empty, positionally numbered blob. Its
/// range is the numeric extrema: `base` is the largest ID counting down.
fn blob_index(
    kind: crate::ElementKind,
    base: i64,
    count: u64,
    bbox: Option<crate::blob_meta::BlobBbox>,
) -> crate::Result<crate::blob_meta::BlobIndex> {
    let last = count
        .checked_sub(1)
        .ok_or_else(|| Error::internal("renumber: an index for an empty blob"))
        .and_then(|span| wire_rewrite::offset_id(base, span))?;
    Ok(crate::blob_meta::BlobIndex {
        kind,
        min_id: base.min(last),
        max_id: base.max(last),
        count,
        bbox,
    })
}

/// The start a kind is numbered from: 0 means 1, as in osmium.
fn effective_start(start: i64) -> i64 {
    if start == 0 { 1 } else { start }
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
/// No temp files. Output uses the compression in `WriteOptions`.
///
/// Refuses input whose node, way or relation IDs do not strictly ascend in
/// canonical order in file order (see the module docs), and a start from
/// which the input's count runs past the 64-bit range.
///
/// The input is read in several passes, so it must be a regular file
/// (`InvalidArgument` otherwise), and is pinned for the run
/// (see [`InputSource::pin`](crate::read::input::InputSource::pin)): a path
/// is pinned here, a caller's [`PinnedInput`] is used as it is. Where a
/// later open cannot reach the pinned file, an input replaced mid-run fails
/// with `InputReplaced`.
pub fn renumber_external<'a>(
    input: impl Into<InputSource<'a>>,
    output: impl crate::output::BindOutput,
    opts: &RenumberOptions,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<RenumberStats> {
    let host = &host.enter()?;
    let output = crate::output::bind_unissued(output, &host.invocation)?;
    host.invocation.check()?;
    let result = renumber_external_to(input, &output, opts, write, overrides, host)?;
    output.publish()?;
    Ok(result)
}

/// [`renumber_external`] writing to a borrowed output it does not publish.
#[allow(clippy::too_many_lines)]
#[hotpath::measure]
pub(crate) fn renumber_external_to<'a>(
    input: impl Into<InputSource<'a>>,
    output: &crate::output::BoundOutput,
    opts: &RenumberOptions,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Entered<'_>,
) -> Result<RenumberStats> {
    let WriteOptions {
        compression,
        backend,
        direct_reads: direct_io,
        ..
    } = *write;
    let start_node_id = effective_start(opts.start_node_id);
    let start_way_id = effective_start(opts.start_way_id);
    let start_relation_id = effective_start(opts.start_relation_id);

    // The input is opened three times, each a new open file description
    // derived from this one pin, so a file renamed over the input mid-run
    // cannot be read partly from each: the header read below, the one
    // description every data phase preads, and the schedule walk's own
    // description (`FADV_RANDOM` when buffered). Under direct reads all
    // three are `O_DIRECT`. Where `/proc/self/fd` reopens the pin every
    // open reaches the pinned file, otherwise on Unix a replaced input fails
    // the run (`PinnedInput` says what it does off Unix). Taken before any
    // other open of it.
    let pin: PinnedInput = input.into().pin(host)?;
    let input = pin.path();

    // ---- Header validation + output writer setup ----
    // The reader reports a file without a leading OsmHeader through the one
    // missing-header error, ahead of the sortedness check.
    let header = crate::ElementReader::open_in(&pin, direct_io, host)?
        .header()
        .clone();
    require_sorted(
        header.is_sorted(),
        input,
        "Input PBF",
        "Renumber assigns IDs by file position and resolves references by rank.",
    )?;
    // The writer fills the output's staging file; `renumber_external`
    // publishes it only after this function has succeeded.
    // Renumbered content has left the feed's ID space: the feed's diffs
    // address other objects than the ones in this file, so no replication
    // field is carried (osmium renumber writes a fresh header too).
    // The way splice rewrites only the ID and the refs, one new ref per old
    // ref in place, and copies the inline coordinates (fields 9 and 10)
    // beside them, as does every cut piece; the block keeps its granularity
    // and offsets. So a declared `LocationsOnWays` stays true and is kept,
    // trusted from the input like any copied way (osmium drops both the
    // arrays and the declaration). Field 20 bytes ride along undeclared:
    // the pin meaning survives the bijective renumbering, but ADR-0007
    // declares the prepass features only on whole-blob copies, and field 5
    // is not written at all.
    let mut writer = writer_from_header(
        output,
        compression,
        &header,
        &super::output_header::OutputFacts {
            replication: super::output_header::Replication::Drop,
            ..super::output_header::OutputFacts::sorted(
                super::output_header::Ways::ReencodedWithLocations,
            )
        },
        overrides,
        backend,
        host,
    )?;

    // Single shared input description for all phases - pread is
    // concurrent-safe. `O_DIRECT` under direct reads: every worker reads it
    // through its own `ReadBuf`, which reads a direct input in aligned spans.
    let shared_file = crate::read::input::InputFile::for_reads(&pin, direct_io, &host.invocation)?;
    // The path every worker locates its errors against, captured once.
    let source: std::sync::Arc<Path> = std::sync::Arc::from(input);
    let file_len = pin.metadata()?.len();

    host.invocation.marker("RENUMBER_EXT_START");

    // ---- Blob schedule scan ----
    let t_sched = Stopwatch::start();
    let (pass1_schedule, way_schedule, relation_schedule) =
        build_all_blob_schedules(&pin, direct_io, host)?;
    host.invocation
        .counter("renumber_ext_schedule_ms", t_sched.elapsed_ms());

    // ---- Pass 1: parallel node rewrite ----
    host.invocation.marker("RENUMBER_EXT_PASS1_START");
    let pass1_total_nodes: u64 = pass1_schedule
        .iter()
        .fold(0_u64, |acc, t| acc.saturating_add(t.element_count));

    // One shared IdSet, pre-allocated so workers can insert atomically
    // through `&self` (word-batched per blob, `AtomicWordBatch`). Only
    // pages hit by observed IDs materialise, including in overflow chunks.
    // Keep the decision independent of indexdata density claims, which
    // cannot establish which pages will actually be hit. The maximum
    // claimed by the whole schedule is bounded by the file size.
    let mut node_id_set = crate::idset::IdSet::new_paged_atomic();
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

    // Both rewrite stages take the decode-worker width from the host,
    // uncapped. Fixed at 4 and 6 workers they ran exactly that many threads
    // busy, thread-starved rather than bound by per-element work
    // (reference/performance/renumber.md, "Host-sized rewrite stages"). Pass
    // 1 was then capped at 12 on the belief, from Germany, that it waited on
    // the writer past that; on planet at 12 the consumer instead sat idle
    // waiting for results, and host width (30 on a 32-thread host) ran pass
    // 1 about 13 % faster in matched cells, with the writer only becoming
    // the constraint there (renumber.md, "Pass 1 at host width"). Do not
    // reintroduce a fixed cap without a matched planet A/B showing one pays.
    // In-flight memory grows with width through the ordered pool's
    // task-count window, the same as for stage 2d; a bound on that belongs
    // to byte-aware admission, not to a worker count.
    let host_width = crate::host::sized_workers(
        &*host.resources,
        crate::tunables::DECODE_WORKERS_RESERVED,
        crate::tunables::DECODE_WORKERS_MIN,
        crate::tunables::DECODE_WORKERS_FALLBACK,
    );
    let pass1_width = host_width.min(pass1_schedule.len()).max(1);
    let stage2d_width = host_width.min(way_schedule.len()).max(1);

    let pass1 = pass1_parallel_scan(
        &pass1_schedule,
        start_node_id,
        &shared_file,
        &source,
        &node_id_set,
        &mut writer,
        pass1_width,
    )?;
    if pass1.nodes_written != pass1_total_nodes {
        return Err(Error::internal(format!(
            "pass1 node count mismatch: schedule reported {pass1_total_nodes}, \
             workers wrote {}",
            pass1.nodes_written,
        )));
    }
    // IDs the pre-allocation did not cover, inserted now that no worker
    // holds the set: negative IDs, and non-negative ones above the
    // pre-allocation. Already checked strictly ascending.
    let mut overflow_ids: u64 = 0;
    for ids in pass1.overflow {
        overflow_ids = overflow_ids.saturating_add(u64::try_from(ids.len()).unwrap_or(u64::MAX));
        for id in ids {
            node_id_set.insert(id);
        }
    }
    host.invocation
        .counter("renumber_ext_node_overflow_ids", overflow_ids);
    host.invocation.marker("RENUMBER_EXT_PASS1_END");

    // ---- Build rank index ----
    // Stage boundaries are checkpoints: the rank builds between the parallel
    // stages have none of their own. The build is serial on purpose: on
    // planet it takes tens of milliseconds for nodes and single digits for
    // ways (the `renumber_ext_*_rank_ms` counters), so parallel rank
    // construction or worker-owned map pages would buy nothing here; see
    // reference/performance/renumber.md "Word-batched ID set insertion".
    host.invocation.check()?;
    let t_rank = Stopwatch::start();
    let node_id_set = node_id_set.into_ranked();
    host.invocation
        .counter("renumber_ext_node_rank_ms", t_rank.elapsed_ms());
    host.invocation
        .counter("renumber_ext_node_map_entries", node_id_set.total_count());
    set_matches_written("node", node_id_set.total_count(), pass1.nodes_written)?;
    let nodes = IdSpace::new(&node_id_set, start_node_id);

    // ---- Stage 2d: fused way resolve + rewrite (single pass) ----
    host.invocation.check()?;
    host.invocation.marker("RENUMBER_EXT_STAGE2D_START");
    // One shared way IdSet, pre-allocated like the node set above.
    let mut way_id_set = crate::idset::IdSet::new_paged_atomic();
    if let Some(claimed_max) = way_schedule.iter().map(|t| t.max_id).max() {
        let bound = crate::idset::IdSet::file_bounded_max_id(
            claimed_max,
            file_len,
            RENUMBER_PREALLOC_IDS_PER_BYTE,
        );
        if bound >= 0 {
            way_id_set.pre_allocate(bound);
        }
    }
    let stage2d = stage2d_parallel_way_assembly(
        &shared_file,
        &source,
        &mut writer,
        &way_id_set,
        &way_schedule,
        &nodes,
        start_way_id,
        stage2d_width,
    )?;
    host.invocation.check()?;
    let mut way_overflow_ids: u64 = 0;
    for ids in stage2d.overflow {
        way_overflow_ids =
            way_overflow_ids.saturating_add(u64::try_from(ids.len()).unwrap_or(u64::MAX));
        for id in ids {
            way_id_set.insert(id);
        }
    }
    host.invocation
        .counter("renumber_ext_way_overflow_ids", way_overflow_ids);
    host.invocation.marker("RENUMBER_EXT_STAGE2D_END");

    // ---- R1: collect relation IDs into IdSet ----
    host.invocation.marker("RENUMBER_EXT_R1_START");
    let t_way_rank = Stopwatch::start();
    let way_id_set = way_id_set.into_ranked();
    host.invocation
        .counter("renumber_ext_way_rank_ms", t_way_rank.elapsed_ms());
    host.invocation
        .counter("renumber_ext_way_map_entries", way_id_set.total_count());
    set_matches_written("way", way_id_set.total_count(), stage2d.ways_written)?;

    let mut relation_id_set = crate::idset::IdSet::new();
    relation_r1_collect_ids(
        &shared_file,
        &source,
        &relation_schedule,
        &mut relation_id_set,
        &host.invocation,
    )?;
    let t_rel_rank = Stopwatch::start();
    let relation_id_set = relation_id_set.into_ranked();
    host.invocation
        .counter("renumber_ext_rel_rank_ms", t_rel_rank.elapsed_ms());
    let relation_count = relation_id_set.total_count();
    host.invocation
        .counter("renumber_ext_relation_map_entries", relation_count);
    // Relation IDs are the start moved by the rank; the largest rank is
    // count - 1.
    if relation_count > 0 {
        positional_last_fits(start_relation_id, relation_count, "relation")?;
    }
    host.invocation.marker("RENUMBER_EXT_R1_END");

    // ---- R2d: parallel wire-format rewrite of relations ----
    host.invocation.marker("RENUMBER_EXT_R2D_START");
    let r2d = relation_r2d_assembly(
        &shared_file,
        &source,
        &relation_schedule,
        &mut writer,
        &wire_rewrite::RelationIdSpaces {
            relations: IdSpace::new(&relation_id_set, start_relation_id),
            nodes,
            ways: IdSpace::new(&way_id_set, start_way_id),
        },
        host,
    )?;
    // Relations are numbered by rank, not position, so this is not about
    // numbering: a relation R1 collected but R2d did not write would leave
    // refs to it resolving as live while it is absent from the output.
    set_matches_written("relation", relation_count, r2d.relations_written)?;
    host.invocation.marker("RENUMBER_EXT_R2D_END");

    writer.flush()?;

    host.invocation.marker("RENUMBER_EXT_END");

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
        .is_some_and(|span| wire_rewrite::offset_id(start, span).is_ok());
    if fits {
        Ok(())
    } else {
        Err(Error::invalid_data(format!(
            "renumber: {kind} start id {start} plus the input's {count} {kind}s \
             runs past the 64-bit id range"
        )))
    }
}

/// Refuse a run whose ID set and written count for `kind` disagree. A
/// reference resolves by rank in the set while an element is numbered by
/// position, and orphans are named around a live range as long as the
/// set's cardinality: if the two counts differed, a live ref could name the
/// wrong element and an orphan could land inside the renumbered range. The
/// order checks already make them equal; this states the invariant the
/// orphan naming rests on.
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
        crate::test_support::assert_err_kind!(
            positional_bases(i64::MAX, &[task(2)], "node"),
            crate::ErrorKind::InvalidData { msg, .. } if msg.contains("runs past the 64-bit id range"),
        );
    }

    #[test]
    fn a_negative_start_counts_down_and_zero_means_one() {
        assert_eq!(effective_start(0), 1);
        assert_eq!(effective_start(-5), -5);
        let bases = positional_bases(-10, &[task(3), task(0), task(2)], "node").expect("fits");
        assert_eq!(bases, vec![-10, -13, -13]);
        assert!(positional_bases(i64::MIN, &[task(1), task(0)], "node").is_ok());
        crate::test_support::assert_err_kind!(
            positional_bases(i64::MIN, &[task(2)], "node"),
            crate::ErrorKind::InvalidData { msg, .. } if msg.contains("runs past the 64-bit id range"),
        );
        let index = blob_index(crate::ElementKind::Way, -10, 3, None).expect("index");
        assert_eq!((index.min_id, index.max_id), (-12, -10));
        assert!(positional_last_fits(i64::MIN + 1, 2, "relation").is_ok());
        crate::test_support::assert_err_kind!(
            positional_last_fits(i64::MIN + 1, 3, "relation"),
            crate::ErrorKind::InvalidData { msg, .. } if msg.contains("runs past the 64-bit id range"),
        );
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
