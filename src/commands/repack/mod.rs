//! Re-encode a PBF with a configurable per-blob element cap.
//!
//! Primary consumer: the blob-density measurement matrix in
//! [`reference/blob-density.md`](../../../reference/blob-density.md). That
//! matrix needs same-corpus-different-encoding pairs to control for blob-
//! count effects independent of byte size; `repack` is the only way to
//! produce them.
//!
//! Implementation: parallel three-phase scan (nodes, ways, relations).
//! Each worker decodes one input blob, filters to the current kind, and
//! splits its matching elements at the per-input-blob `M % cap` boundary:
//!
//! - The first `M - (M % cap)` elements (a multiple of `cap`) are
//!   re-encoded through a per-worker `BlockBuilder` and shipped to the
//!   merge thread as already-framed blob bytes. This is the parallel
//!   path that recovers v1's shrink throughput.
//! - The remaining `M % cap` elements (the trailing partial) are shipped
//!   as decoded `Owned*` data to the merge thread.
//!
//! The merge thread runs a single long-lived `BlockBuilder` per kind,
//! configured with the requested cap, and consumes payloads in seq order
//! from the ordered classify pool. Per input blob, when that blob carries direct
//! full blocks it first drains the central stream (flush the builder,
//! write all pending) so the earlier lower-ID tails precede this blob's
//! higher-ID full blocks, then writes the worker's full framed blocks
//! directly, then feeds the trailing `Owned*` slice into the central
//! builder; mid-stream flushes are framed in parallel (`rayon::par_iter`
//! over `FRAME_BATCH`-sized batches) and written serially. The drain
//! guard keeps the two output streams globally `Sort.Type_then_ID`
//! ordered; on a pure grow (empty `full_framed`) it never fires, so the
//! central builder still coalesces across input-blob boundaries.
//!
//! Both shrink (planet's ~228 k/blob -> 8 k/blob) and grow (Geofabrik
//! 8 k/blob -> 64 k/blob) work because the central builder spans
//! input-blob boundaries on the trailing path. The
//! `repack_input_blobs_coalesced` counter tracks how often a non-empty
//! trailing payload extends a non-empty central builder. The
//! "never fired" warning fires only when the cap exceeds every kind's
//! total element count (every kind collapses to a single output blob).

use super::{
    HeaderOverrides, Result, ensure_node_capacity_local, ensure_relation_capacity_local,
    ensure_way_capacity_local, flush_local, require_indexdata, writer_from_header,
};
use crate::ElementKind;
use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::owned::{
    OwnedNode, OwnedRelation, OwnedWay, dense_node_metadata, element_metadata, read_dense_node,
    read_node, read_relation, read_way, read_way_with_locations, relation_members_into,
    write_single_node_local, write_single_relation_local, write_single_way_local,
};
use crate::read::input::InputSource;
use crate::write::WriteOptions;
use crate::writer::{Compression, PbfWriter, frame_blob_pipelined};
use crate::{Element, ElementReader};

use crate::tunables::FRAME_BATCH;

/// Per-run statistics from a repack operation.
pub struct RepackStats {
    pub blobs_written: u64,
    pub elements_written: u64,
    pub elements_per_blob: usize,
}

impl RepackStats {
    pub fn write_summary(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        writeln!(
            w,
            "Repacked {} elements into {} blobs (cap {} elements/blob)",
            self.elements_written, self.blobs_written, self.elements_per_blob,
        )?;

        Ok(())
    }
}

/// Trailing-partial payload: 0 to `cap-1` elements that didn't fill a
/// full output block in the worker. One variant populated per phase.
enum KindPayload {
    Nodes(Vec<OwnedNode>),
    Ways(Vec<OwnedWay>),
    Relations(Vec<OwnedRelation>),
}

impl KindPayload {
    fn len(&self) -> u64 {
        let n = match self {
            Self::Nodes(v) => v.len(),
            Self::Ways(v) => v.len(),
            Self::Relations(v) => v.len(),
        };
        n as u64
    }
}

/// One worker's output for one input blob: framed full blocks plus the
/// trailing partial.
struct WorkerOutput {
    full_framed: Vec<Vec<u8>>,
    tail: KindPayload,
}

/// Re-encode `input` to `output` with a per-blob element cap of
/// `elements_per_blob`. Element semantics are preserved: every element
/// round-trips with its tags, refs, members, metadata, and DenseNodes
/// encoding. Output is type-sorted (nodes, then ways, then relations);
/// the `Sort.Type_then_ID` flag is propagated when the input has it.
///
/// The input is read in several passes, so it must be a regular file
/// (`InvalidArgument` otherwise), and is pinned for the run
/// (see [`PinnedInput`](crate::read::input::PinnedInput)); a caller's pin is
/// used as it is. Where a later open cannot reach the pinned file, an input
/// replaced mid-run fails with `InputReplaced` and nothing is published.
pub fn repack<'a>(
    input: impl Into<InputSource<'a>>,
    output: crate::output::Output,
    elements_per_blob: std::num::NonZeroUsize,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<RepackStats> {
    host.invocation.check()?;
    let result = repack_to(
        input.into(),
        &output,
        elements_per_blob,
        write,
        overrides,
        host,
    )?;
    output.publish(&host.invocation)?;
    Ok(result)
}

/// [`repack`] writing to a borrowed output it does not publish.
#[allow(clippy::too_many_lines)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(crate) fn repack_to(
    input: InputSource<'_>,
    output: &crate::output::Output,
    cap: std::num::NonZeroUsize,
    write: &WriteOptions,
    overrides: &HeaderOverrides,
    host: &crate::host::Host,
) -> Result<RepackStats> {
    let WriteOptions {
        compression,
        backend,
        direct_reads: direct_io,
        force,
    } = *write;
    let elements_per_blob = cap.get();

    // The input is opened three times, each through its own open file
    // description: the indexdata probe, the header reader, then the schedule
    // walk whose descriptor every per-kind pool preads. All three are derived
    // from this one pin, and the size bound below is read from it, so a
    // file renamed over the input mid-run cannot be probed, headed and
    // re-encoded partly from each file: where `/proc/self/fd` reopens the pin
    // every open reaches the pinned file, otherwise on Unix a replaced input
    // fails the run (`PinnedInput` says what it does off Unix). Taken before
    // any other open of the input.
    let pin = input.pin(host)?;
    let input = &pin;

    require_indexdata(
        input,
        direct_io,
        force,
        "input PBF has no blob-level indexdata. repack uses the parallel \
         per-kind classify pipeline, which needs indexdata to build per-kind \
         blob schedules.",
        host,
    )?;

    // Per-blob worker pool; same precedent as `cat --clean`.
    host.process.cross_thread_alloc_pool();

    // Unlike tags-filter / extract, repack round-trips way payloads with
    // their inline coordinates, so an input declaring LocationsOnWays keeps
    // it: the way phase threads the coordinates through both the worker
    // full-block path and the trailing Owned* merge path. An input without
    // LocationsOnWays carries no coordinates and the output is the plain
    // shape (no implicit conversion). The prepass fields are not rewritten,
    // so their declarations go, with the shared warning.
    let (header, preserve_locations) = {
        let reader = ElementReader::open_in(input, direct_io, host)?;
        let preserve = reader.header().has_locations_on_ways();
        (reader.header().clone(), preserve)
    };
    let mut writer = writer_from_header(
        output,
        compression,
        &header,
        &super::output_header::OutputFacts::carry(
            super::output_header::Ways::ReencodedWithLocations,
        ),
        overrides,
        backend,
        host,
    )?;

    // The fifth result sizes IdSets in check callers; repack only needs the
    // schedules.
    let (node_schedule, way_schedule, rel_schedule, shared_file, _) =
        crate::scan::classify::build_classify_schedules_split(input, host)?;

    crate::debug::emit_counter("repack_input_blobs_nodes", node_schedule.len());
    crate::debug::emit_counter("repack_input_blobs_ways", way_schedule.len());
    crate::debug::emit_counter("repack_input_blobs_relations", rel_schedule.len());
    crate::debug::emit_counter(
        "repack_input_blobs_total",
        node_schedule.len() + way_schedule.len() + rel_schedule.len(),
    );

    // Pre-allocation ceiling for every BlockBuilder in this run, derived from
    // the input rather than fixed: a file cannot deliver more elements than it
    // has bytes. See `BlockBuilder::with_element_cap_prealloc`. The size is
    // the pinned file's, and a failed stat of it fails the run: falling back
    // to an unbounded ceiling would drop the bound exactly when it is unknown.
    let prealloc_bound = usize::try_from(input.metadata()?.len()).unwrap_or(usize::MAX);

    let mut blobs_written: u64 = 0;
    let mut elements_written: u64 = 0;
    let mut total_coalesces: u64 = 0;
    let mut total_worker_full_blobs: u64 = 0;
    let mut total_central_blobs: u64 = 0;
    let mut any_cap_fired: bool = false;

    crate::debug::emit_marker("REPACK_NODES_START");
    let node_stats = run_kind_phase(
        &shared_file,
        &node_schedule,
        ElementKind::Node,
        cap,
        prealloc_bound,
        compression,
        preserve_locations,
        &mut writer,
        host,
    )?;
    crate::debug::emit_marker("REPACK_NODES_END");
    emit_phase_counters(ElementKind::Node, &node_stats);
    blobs_written += node_stats.blobs;
    elements_written += node_stats.elements;
    total_coalesces += node_stats.coalesces;
    total_worker_full_blobs += node_stats.worker_full_blobs;
    total_central_blobs += node_stats.central_blobs;
    any_cap_fired |= node_stats.cap_fired;

    crate::debug::emit_marker("REPACK_WAYS_START");
    let way_stats = run_kind_phase(
        &shared_file,
        &way_schedule,
        ElementKind::Way,
        cap,
        prealloc_bound,
        compression,
        preserve_locations,
        &mut writer,
        host,
    )?;
    crate::debug::emit_marker("REPACK_WAYS_END");
    emit_phase_counters(ElementKind::Way, &way_stats);
    blobs_written += way_stats.blobs;
    elements_written += way_stats.elements;
    total_coalesces += way_stats.coalesces;
    total_worker_full_blobs += way_stats.worker_full_blobs;
    total_central_blobs += way_stats.central_blobs;
    any_cap_fired |= way_stats.cap_fired;

    crate::debug::emit_marker("REPACK_RELATIONS_START");
    let rel_stats = run_kind_phase(
        &shared_file,
        &rel_schedule,
        ElementKind::Relation,
        cap,
        prealloc_bound,
        compression,
        preserve_locations,
        &mut writer,
        host,
    )?;
    crate::debug::emit_marker("REPACK_RELATIONS_END");
    emit_phase_counters(ElementKind::Relation, &rel_stats);
    blobs_written += rel_stats.blobs;
    elements_written += rel_stats.elements;
    total_coalesces += rel_stats.coalesces;
    total_worker_full_blobs += rel_stats.worker_full_blobs;
    total_central_blobs += rel_stats.central_blobs;
    any_cap_fired |= rel_stats.cap_fired;

    crate::debug::emit_marker("REPACK_FLUSH_START");
    writer.flush()?;
    crate::debug::emit_marker("REPACK_FLUSH_END");

    crate::debug::emit_counter("repack_blobs_written", blobs_written);
    crate::debug::emit_counter("repack_elements_written", elements_written);
    crate::debug::emit_counter("repack_input_blobs_coalesced", total_coalesces);
    crate::debug::emit_counter("repack_worker_full_blobs", total_worker_full_blobs);
    crate::debug::emit_counter("repack_central_blobs", total_central_blobs);
    crate::debug::emit_counter("repack_cap_fired", any_cap_fired);
    crate::debug::emit_counter("repack_elements_per_blob", elements_per_blob);

    // Detect the silent-identity surprise: cap exceeds every kind's total
    // element count, so no kind has ever flushed (in workers or in the
    // central builder) and the output is one blob per non-empty kind.
    // Only meaningful when there was real work to do.
    if !any_cap_fired && elements_written > 0 {
        crate::host::diag!(
            host.diagnostics,
            Warning,
            "repack.cap_never_fired",
            "the element cap of {elements_per_blob} per blob never fired; \
             every per-kind element count fits in a single output blob, \
             so the output is one blob per kind."
        );
    }

    Ok(RepackStats {
        blobs_written,
        elements_written,
        elements_per_blob,
    })
}

/// Per-kind phase totals returned to the top-level `repack` driver.
struct PhaseStats {
    blobs: u64,
    elements: u64,
    /// Count of input blobs whose trailing payload extended a non-empty
    /// central builder. 0 on shrinks with exact division; otherwise grows
    /// with the proportion of input blobs that don't divide cleanly.
    coalesces: u64,
    /// Output blobs framed in workers (full-block path).
    worker_full_blobs: u64,
    /// Output blobs framed via the merge-thread central builder (mid-stream
    /// flushes from cap fires + the final residual flush).
    central_blobs: u64,
    /// True iff this kind produced more than one output blob, i.e. the
    /// cap actually shaped the output (worker emitted full blocks or the
    /// central builder flushed mid-stream). Used by the global warning.
    cap_fired: bool,
}

/// Sidecar counter names for one kind's phase, in emission order: blobs,
/// elements, coalesces, worker_full_blobs, central_blobs, cap_fired. Spelled
/// out literally so each name can be grepped verbatim.
fn phase_counter_names(kind: ElementKind) -> [&'static str; 6] {
    match kind {
        ElementKind::Node => [
            "repack_nodes_blobs",
            "repack_nodes_elements",
            "repack_nodes_coalesces",
            "repack_nodes_worker_full_blobs",
            "repack_nodes_central_blobs",
            "repack_nodes_cap_fired",
        ],
        ElementKind::Way => [
            "repack_ways_blobs",
            "repack_ways_elements",
            "repack_ways_coalesces",
            "repack_ways_worker_full_blobs",
            "repack_ways_central_blobs",
            "repack_ways_cap_fired",
        ],
        ElementKind::Relation => [
            "repack_relations_blobs",
            "repack_relations_elements",
            "repack_relations_coalesces",
            "repack_relations_worker_full_blobs",
            "repack_relations_central_blobs",
            "repack_relations_cap_fired",
        ],
    }
}

/// Emit per-kind sidecar counters for one `run_kind_phase` result.
fn emit_phase_counters(kind: ElementKind, s: &PhaseStats) {
    let [blobs, elements, coalesces, worker_full, central, cap_fired] = phase_counter_names(kind);
    crate::debug::emit_counter(blobs, s.blobs);
    crate::debug::emit_counter(elements, s.elements);
    crate::debug::emit_counter(coalesces, s.coalesces);
    crate::debug::emit_counter(worker_full, s.worker_full_blobs);
    crate::debug::emit_counter(central, s.central_blobs);
    crate::debug::emit_counter(cap_fired, s.cap_fired);
}

/// Run one per-kind phase: pread workers decode + split (full blocks
/// framed in parallel; trailing partial owned-and-shipped); the merge
/// thread writes full blocks directly and runs a single long-lived
/// `BlockBuilder` for cross-input-blob coalescing on the trailing slices.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn run_kind_phase(
    shared_file: &crate::read::input::InputFile,
    schedule: &[crate::scan::classify::ScheduledBlob],
    kind: ElementKind,
    cap: std::num::NonZeroUsize,
    prealloc_bound: usize,
    compression: Compression,
    preserve_locations: bool,
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    host: &crate::host::Host,
) -> Result<PhaseStats> {
    let elements_per_blob = cap.get();

    if schedule.is_empty() {
        return Ok(PhaseStats {
            blobs: 0,
            elements: 0,
            coalesces: 0,
            worker_full_blobs: 0,
            central_blobs: 0,
            cap_fired: false,
        });
    }

    type PhaseResult = crate::Result<WorkerOutput>;

    // Same bound as the workers', derived from the input instead of a block:
    // the file cannot deliver more elements than it has bytes (an element
    // needs at least one byte on the wire). Undershooting only costs a vector
    // growth that in practice never happens; not bounding it at all let a
    // single CLI integer request an unbounded allocation.
    let mut bb = BlockBuilder::with_element_cap_prealloc(cap, prealloc_bound);
    let mut output: Vec<OwnedBlock> = Vec::new();
    let mut pending: Vec<OwnedBlock> = Vec::with_capacity(FRAME_BATCH);
    let mut blobs: u64 = 0;
    let mut elements: u64 = 0;
    let mut coalesces: u64 = 0;
    let mut worker_full_blobs: u64 = 0;
    let mut central_blobs: u64 = 0;

    // Results arrive in schedule order through the ordered pool, whose window
    // bounds the decoded payloads in flight. A write or encode error ends the
    // phase at its position.
    crate::scan::classify::parallel_classify_phase_ordered(
        shared_file,
        schedule,
        None,
        None,
        || (),
        |block, _state| -> PhaseResult {
            worker_split_blob(
                block,
                kind,
                cap,
                preserve_locations,
                &compression,
                &host.invocation.metrics().writer,
            )
        },
        |_position, item| {
            let WorkerOutput { full_framed, tail } = item;
            let full_count = full_framed.len() as u64 * elements_per_blob as u64;
            let tail_n = tail.len();

            // Ordering guard (restores Sort.Type_then_ID monotonicity
            // across the two output streams). This input blob's full
            // blocks carry IDs strictly above every tail accumulated so
            // far: the source is type-then-ID sorted, and the worker
            // splits the low `M - M%cap` IDs into `full_framed` and the
            // high `M%cap` IDs into `tail`, so blob N's full blocks
            // outrank blob N-1's tail. The central stream (`bb` +
            // `pending`) holds those earlier lower-ID tails but is
            // otherwise flushed only in FRAME_BATCH-sized batches - a
            // delayed batch would land AFTER these direct writes and
            // emit lower IDs after higher ones. So before any direct
            // write, drain the central stream: flush `bb`, then write
            // everything `pending` holds. This leaves pure-grow
            // coalescing untouched - grows produce empty `full_framed`,
            // so the guard never fires and the central builder still
            // coalesces trailing slices across input-blob boundaries.
            // In the shrink/mixed case it splits each accumulated tail
            // into its own (possibly under-cap) block, which is the
            // correct trade: coalescing there was buying little and was
            // the source of the reordering.
            if !full_framed.is_empty() && (!bb.is_empty() || !pending.is_empty()) {
                flush_local(&mut bb, &mut output)?;
                central_blobs += output.len() as u64;
                pending.append(&mut output);
                if !pending.is_empty() {
                    let batch = std::mem::take(&mut pending);
                    blobs += frame_and_write_batch(batch, compression, writer, &host.executor)?;
                }
            }

            // Write the worker's already-framed full blocks directly.
            for framed in full_framed {
                writer.write_raw_owned(framed)?;
                blobs += 1;
                worker_full_blobs += 1;
            }

            // Coalesce check: the trailing slice extends a non-empty
            // central builder iff it's non-empty and the BB carries
            // residual elements from a prior input blob's tail.
            if tail_n > 0 && !bb.is_empty() {
                coalesces += 1;
            }

            match tail {
                KindPayload::Nodes(nodes) => {
                    for node in &nodes {
                        write_single_node_local(node, &mut bb, &mut output)?;
                    }
                }
                KindPayload::Ways(ways) => {
                    for way in &ways {
                        write_single_way_local(way, &mut bb, &mut output)?;
                    }
                }
                KindPayload::Relations(rels) => {
                    for rel in &rels {
                        write_single_relation_local(rel, &mut bb, &mut output)?;
                    }
                }
            }
            // Anything in `output` came from a mid-stream cap fire on
            // the central builder; track for the warning logic, then
            // accumulate into `pending` for batched parallel framing.
            central_blobs += output.len() as u64;
            pending.append(&mut output);
            while pending.len() >= FRAME_BATCH {
                let batch: Vec<OwnedBlock> = pending.drain(..FRAME_BATCH).collect();
                blobs += frame_and_write_batch(batch, compression, writer, &host.executor)?;
            }
            elements += full_count + tail_n;
            Ok(())
        },
        host,
    )?;

    // Final flush: emit the residual block (if any) for this kind. Any
    // blocks `output` holds at this point are residuals, not mid-stream
    // flushes, so they don't count toward `central_blobs`.
    flush_local(&mut bb, &mut output)?;
    let residual = output.len() as u64;
    pending.append(&mut output);
    if !pending.is_empty() {
        let final_batch = std::mem::take(&mut pending);
        let written = frame_and_write_batch(final_batch, compression, writer, &host.executor)?;
        blobs += written;
    }

    // Cap fired iff this kind produced more than one output blob. The
    // residual final-flush adds at most one block; everything else
    // (worker full blobs + central mid-stream flushes) reflects the cap
    // actually shaping the output.
    let cap_fired = (worker_full_blobs + central_blobs + residual) > 1;

    // Roll the residual into central_blobs for the sidecar accounting:
    // the merge thread framed and wrote it via the same `frame_and_write_batch`
    // path as the mid-stream flushes.
    let central_blobs_total = central_blobs + residual;

    Ok(PhaseStats {
        blobs,
        elements,
        coalesces,
        worker_full_blobs,
        central_blobs: central_blobs_total,
        cap_fired,
    })
}

/// Encode one way into the per-worker full-block `BlockBuilder`, reusing the
/// caller's scratch buffers. When `preserve_locations` is set (input header
/// declares LocationsOnWays) the way's own inline coordinates are embedded via
/// `add_way_with_locations`. The length guard keeps a way that carries no
/// inline coordinates under a LOW header (empty lat/lon fields) on the plain
/// `add_way` path instead of tripping the refs==locations invariant.
fn encode_way_full_block(
    bb: &mut BlockBuilder,
    w: &crate::Way<'_>,
    refs_buf: &mut Vec<i64>,
    locations_buf: &mut Vec<(i32, i32)>,
    preserve_locations: bool,
) -> crate::Result<()> {
    refs_buf.clear();
    refs_buf.extend(w.refs());
    let meta = element_metadata(ElementKind::Way, w.id(), &w.info())?;
    if preserve_locations {
        locations_buf.clear();
        locations_buf.extend(
            w.node_locations()
                .map(|loc| (loc.decimicro_lat(), loc.decimicro_lon())),
        );
    }
    if preserve_locations && locations_buf.len() == refs_buf.len() && !locations_buf.is_empty() {
        bb.add_way_with_locations(w.id(), w.tags(), refs_buf, locations_buf, meta.as_ref());
    } else {
        bb.add_way(w.id(), w.tags(), refs_buf, meta.as_ref());
    }
    Ok(())
}

/// One worker's body: count matching elements, frame the leading
/// `M - M%cap` elements as full blocks via a per-worker `BlockBuilder`,
/// and ship the trailing `M%cap` elements as `Owned*` data for the merge
/// thread to coalesce across input-blob boundaries.
#[allow(clippy::too_many_lines)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn worker_split_blob(
    block: &crate::PrimitiveBlock,
    kind: ElementKind,
    cap_nz: std::num::NonZeroUsize,
    preserve_locations: bool,
    compression: &Compression,
    metrics: &crate::write::metrics::WriterMetrics,
) -> crate::Result<WorkerOutput> {
    let cap = cap_nz.get();
    // Trust: the schedule put this blob in this kind's phase because its
    // indexdata names the kind, and only elements of that kind are counted and
    // re-encoded below. Elements of any other kind in the blob (a mislabelled
    // indexdata, or a valid multi-kind block indexed by another writer's rule)
    // are dropped without a count or an error. Checking per element costs on a
    // measured path, and a per-blob check here cannot tell such a blob from an
    // unindexed one the schedule builder replicated into every kind's phase,
    // where a no-match is correct. See reference/pipeline.md "Blob Filtering".
    let total = match kind {
        ElementKind::Node => block
            .elements()
            .filter(|e| matches!(e, Element::DenseNode(_) | Element::Node(_)))
            .count(),
        ElementKind::Way => block
            .elements()
            .filter(|e| matches!(e, Element::Way(_)))
            .count(),
        ElementKind::Relation => block
            .elements()
            .filter(|e| matches!(e, Element::Relation(_)))
            .count(),
    };
    let tail_size = total % cap;
    let full_count = total - tail_size;

    // `cap` is user input; `total` is what this block actually holds. A block
    // can never fill more than `total` slots, so that is the honest
    // pre-allocation bound - `repack --elements-per-blob 9000000000000000000`
    // must not ask for a 9-exabyte vector per worker.
    let mut bb = BlockBuilder::with_element_cap_prealloc(cap_nz, total);
    let mut output: Vec<OwnedBlock> = Vec::new();
    let mut full_framed: Vec<Vec<u8>> = Vec::new();
    let mut tail: KindPayload = match kind {
        ElementKind::Node => KindPayload::Nodes(Vec::with_capacity(tail_size)),
        ElementKind::Way => KindPayload::Ways(Vec::with_capacity(tail_size)),
        ElementKind::Relation => KindPayload::Relations(Vec::with_capacity(tail_size)),
    };

    let mut idx: usize = 0;
    let mut refs_buf: Vec<i64> = Vec::new();
    let mut locations_buf: Vec<(i32, i32)> = Vec::new();
    let mut members_buf: Vec<MemberData<'_>> = Vec::new();
    for element in block.elements() {
        let is_match = matches!(
            (&element, kind),
            (Element::DenseNode(_) | Element::Node(_), ElementKind::Node)
                | (Element::Way(_), ElementKind::Way)
                | (Element::Relation(_), ElementKind::Relation)
        );
        if !is_match {
            continue;
        }

        if idx < full_count {
            // Full-block path: re-encode through the per-worker builder
            // exactly like v1. Frame any output produced by `ensure_*`.
            match (&element, kind) {
                (Element::DenseNode(dn), ElementKind::Node) => {
                    ensure_node_capacity_local(&mut bb, &mut output)?;
                    let meta = dense_node_metadata(dn)?;
                    bb.add_node(
                        dn.id(),
                        dn.decimicro_lat(),
                        dn.decimicro_lon(),
                        dn.tags(),
                        meta.as_ref(),
                    );
                }
                (Element::Node(n), ElementKind::Node) => {
                    ensure_node_capacity_local(&mut bb, &mut output)?;
                    let meta = element_metadata(ElementKind::Node, n.id(), &n.info())?;
                    bb.add_node(
                        n.id(),
                        n.decimicro_lat(),
                        n.decimicro_lon(),
                        n.tags(),
                        meta.as_ref(),
                    );
                }
                (Element::Way(w), ElementKind::Way) => {
                    ensure_way_capacity_local(&mut bb, &mut output)?;
                    encode_way_full_block(
                        &mut bb,
                        w,
                        &mut refs_buf,
                        &mut locations_buf,
                        preserve_locations,
                    )?;
                }
                (Element::Relation(r), ElementKind::Relation) => {
                    ensure_relation_capacity_local(&mut bb, &mut output)?;
                    relation_members_into(r, &mut members_buf)?;
                    let meta = element_metadata(ElementKind::Relation, r.id(), &r.info())?;
                    bb.add_relation(r.id(), r.tags(), &members_buf, meta.as_ref());
                }
                _ => {}
            }
            for owned_block in output.drain(..) {
                full_framed.push(frame_owned(owned_block, compression, metrics)?);
            }
        } else {
            // Tail path: own and ship to the merge thread.
            match (&element, &mut tail) {
                (Element::DenseNode(dn), KindPayload::Nodes(v)) => v.push(read_dense_node(dn)?),
                (Element::Node(n), KindPayload::Nodes(v)) => v.push(read_node(n)?),
                (Element::Way(w), KindPayload::Ways(v)) => v.push(if preserve_locations {
                    read_way_with_locations(w)?
                } else {
                    read_way(w)?
                }),
                (Element::Relation(r), KindPayload::Relations(v)) => v.push(read_relation(r)?),
                _ => {}
            }
        }
        idx += 1;
    }

    // Force-flush the final full block (if any). After processing
    // exactly `full_count` matching elements and `full_count` is a
    // multiple of `cap`, the BB holds the last cap elements unflushed.
    flush_local(&mut bb, &mut output)?;
    for owned_block in output.drain(..) {
        full_framed.push(frame_owned(owned_block, compression, metrics)?);
    }

    Ok(WorkerOutput { full_framed, tail })
}

/// Frame a single `OwnedBlock` to wire bytes. Worker-side helper; uses
/// the thread-local `PIPELINE_SCRATCH` so it's safe to call from any
/// rayon / `parallel_classify_phase` worker.
fn frame_owned(
    owned: OwnedBlock,
    compression: &Compression,
    metrics: &crate::write::metrics::WriterMetrics,
) -> crate::Result<Vec<u8>> {
    let OwnedBlock {
        bytes: block_bytes,
        index,
        tagdata,
        way_members,
    } = owned;
    let indexdata = index.serialize();
    let blob = frame_blob_pipelined(
        &block_bytes,
        compression,
        Some(indexdata.as_slice()),
        tagdata.as_deref(),
        way_members.as_deref(),
        metrics,
    )?;
    Ok(blob.into_vec())
}

/// Frame `batch` in parallel via rayon, then write the framed bytes in
/// seq order. Used by the merge thread to keep central-builder framing
/// off the serial critical path.
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn frame_and_write_batch(
    batch: Vec<OwnedBlock>,
    compression: Compression,
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    executor: &crate::host::Executor,
) -> Result<u64> {
    use rayon::prelude::*;

    let invocation = writer.cancel_token().clone();
    let metrics = &invocation.metrics().writer;
    let framed: Vec<Result<Vec<u8>>> = executor.install(|| {
        batch
            .into_par_iter()
            .map(
                |OwnedBlock {
                     bytes: block_bytes,
                     index,
                     tagdata,
                     way_members,
                 }|
                 -> Result<Vec<u8>> {
                    let indexdata = index.serialize();
                    let blob = frame_blob_pipelined(
                        &block_bytes,
                        &compression,
                        Some(indexdata.as_slice()),
                        tagdata.as_deref(),
                        way_members.as_deref(),
                        metrics,
                    )?;
                    Ok(blob.into_vec())
                },
            )
            .collect()
    });

    let mut written: u64 = 0;
    for r in framed {
        let bytes = r?;
        writer.write_raw_owned(bytes)?;
        written += 1;
    }
    Ok(written)
}

#[cfg(test)]
mod counter_name_tests {
    use super::*;

    #[test]
    fn phase_counter_names_match_the_old_format_spelling() {
        for (kind, word) in [
            (ElementKind::Node, "nodes"),
            (ElementKind::Way, "ways"),
            (ElementKind::Relation, "relations"),
        ] {
            let want = [
                "blobs",
                "elements",
                "coalesces",
                "worker_full_blobs",
                "central_blobs",
                "cap_fired",
            ]
            .map(|c| format!("repack_{word}_{c}"));
            assert_eq!(phase_counter_names(kind).map(String::from), want);
        }
    }
}
