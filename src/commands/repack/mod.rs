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
//! via a `ReorderBuffer`. Per input blob it writes the worker's full
//! framed blocks directly, then feeds the trailing `Owned*` slice into
//! the central builder; mid-stream flushes are framed in parallel
//! (`rayon::par_iter` over `FRAME_BATCH`-sized batches) and written
//! serially.
//!
//! Both shrink (planet's ~228 k/blob -> 8 k/blob) and grow (Geofabrik
//! 8 k/blob -> 64 k/blob) work because the central builder spans
//! input-blob boundaries on the trailing path. The
//! `repack_input_blobs_coalesced` counter tracks how often a non-empty
//! trailing payload extends a non-empty central builder. The
//! "never fired" warning fires only when the cap exceeds every kind's
//! total element count (every kind collapses to a single output blob).

use std::path::Path;

use super::{
    ensure_node_capacity_local, ensure_relation_capacity_local, ensure_way_capacity_local,
    flush_local, require_indexdata, writer_from_header, HeaderOverrides, Result,
};
use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::owned::{
    dense_node_metadata, element_metadata, read_dense_node, read_node, read_relation, read_way,
    write_single_node_local, write_single_relation_local, write_single_way_local, OwnedNode,
    OwnedRelation, OwnedWay,
};
use crate::writer::{frame_blob_pipelined, Compression, PbfWriter};
use crate::{Element, ElementReader};

const KIND_NODE: u8 = 0;
const KIND_WAY: u8 = 1;
const KIND_RELATION: u8 = 2;

/// Batch size for the parallel-framing fan-out on the merge thread. The
/// merge thread accumulates `OwnedBlock`s in seq order as the central
/// builder flushes; once a batch is full it is framed in parallel via
/// rayon and the framed bytes are written in seq order.
const FRAME_BATCH: usize = 32;

/// Per-run statistics from a repack operation.
pub struct RepackStats {
    pub blobs_written: u64,
    pub elements_written: u64,
    pub elements_per_blob: usize,
}

impl RepackStats {
    pub fn print_summary(&self) {
        eprintln!(
            "Repacked {} elements into {} blobs (cap {} elements/blob)",
            self.elements_written, self.blobs_written, self.elements_per_blob,
        );
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
    fn empty(kind: u8) -> Self {
        match kind {
            KIND_WAY => Self::Ways(Vec::new()),
            KIND_RELATION => Self::Relations(Vec::new()),
            _ => Self::Nodes(Vec::new()),
        }
    }

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
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub fn repack(
    input: &Path,
    output: &Path,
    elements_per_blob: usize,
    compression: Compression,
    direct_io: bool,
    io_uring: bool,
    force: bool,
    overrides: &HeaderOverrides,
) -> Result<RepackStats> {
    if elements_per_blob == 0 {
        return Err("--elements-per-blob must be > 0".into());
    }

    require_indexdata(
        input,
        direct_io,
        force,
        "input PBF has no blob-level indexdata. repack uses the parallel \
         per-kind classify pipeline, which needs indexdata to build per-kind \
         blob schedules.",
    )?;

    // Cap glibc arenas to prevent cross-thread alloc/free fragmentation in
    // the per-blob worker pool. Same precedent as `cat --clean`.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 2);
    }

    let header = {
        let reader = ElementReader::open(input, direct_io)?;
        super::warn_locations_on_ways_loss(reader.header());
        reader.header().clone()
    };
    let mut writer = writer_from_header(
        output,
        compression,
        &header,
        true,
        overrides,
        |hb| hb,
        direct_io,
        io_uring,
    )?;

    let (node_schedule, way_schedule, rel_schedule, shared_file) =
        crate::scan::classify::build_classify_schedules_split(input)?;

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("repack_input_blobs_nodes", node_schedule.len() as i64);
        crate::debug::emit_counter("repack_input_blobs_ways", way_schedule.len() as i64);
        crate::debug::emit_counter("repack_input_blobs_relations", rel_schedule.len() as i64);
        crate::debug::emit_counter(
            "repack_input_blobs_total",
            (node_schedule.len() + way_schedule.len() + rel_schedule.len()) as i64,
        );
    }

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
        KIND_NODE,
        elements_per_blob,
        compression,
        &mut writer,
    )?;
    crate::debug::emit_marker("REPACK_NODES_END");
    emit_phase_counters("nodes", &node_stats);
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
        KIND_WAY,
        elements_per_blob,
        compression,
        &mut writer,
    )?;
    crate::debug::emit_marker("REPACK_WAYS_END");
    emit_phase_counters("ways", &way_stats);
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
        KIND_RELATION,
        elements_per_blob,
        compression,
        &mut writer,
    )?;
    crate::debug::emit_marker("REPACK_RELATIONS_END");
    emit_phase_counters("relations", &rel_stats);
    blobs_written += rel_stats.blobs;
    elements_written += rel_stats.elements;
    total_coalesces += rel_stats.coalesces;
    total_worker_full_blobs += rel_stats.worker_full_blobs;
    total_central_blobs += rel_stats.central_blobs;
    any_cap_fired |= rel_stats.cap_fired;

    crate::debug::emit_marker("REPACK_FLUSH_START");
    writer.flush()?;
    crate::debug::emit_marker("REPACK_FLUSH_END");

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("repack_blobs_written", blobs_written as i64);
        crate::debug::emit_counter("repack_elements_written", elements_written as i64);
        crate::debug::emit_counter("repack_input_blobs_coalesced", total_coalesces as i64);
        crate::debug::emit_counter("repack_worker_full_blobs", total_worker_full_blobs as i64);
        crate::debug::emit_counter("repack_central_blobs", total_central_blobs as i64);
        crate::debug::emit_counter("repack_cap_fired", i64::from(any_cap_fired));
        crate::debug::emit_counter("repack_elements_per_blob", elements_per_blob as i64);
    }

    // Detect the silent-identity surprise: cap exceeds every kind's total
    // element count, so no kind has ever flushed (in workers or in the
    // central builder) and the output is one blob per non-empty kind.
    // Only meaningful when there was real work to do.
    if !any_cap_fired && elements_written > 0 {
        eprintln!(
            "Warning: --elements-per-blob {elements_per_blob} never fired; \
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

/// Emit per-kind sidecar counters for one `run_kind_phase` result.
fn emit_phase_counters(kind: &str, s: &PhaseStats) {
    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter(&format!("repack_{kind}_blobs"), s.blobs as i64);
        crate::debug::emit_counter(&format!("repack_{kind}_elements"), s.elements as i64);
        crate::debug::emit_counter(&format!("repack_{kind}_coalesces"), s.coalesces as i64);
        crate::debug::emit_counter(
            &format!("repack_{kind}_worker_full_blobs"),
            s.worker_full_blobs as i64,
        );
        crate::debug::emit_counter(
            &format!("repack_{kind}_central_blobs"),
            s.central_blobs as i64,
        );
        crate::debug::emit_counter(
            &format!("repack_{kind}_cap_fired"),
            i64::from(s.cap_fired),
        );
    }
}

/// Run one per-kind phase: pread workers decode + split (full blocks
/// framed in parallel; trailing partial owned-and-shipped); the merge
/// thread writes full blocks directly and runs a single long-lived
/// `BlockBuilder` for cross-input-blob coalescing on the trailing slices.
#[allow(clippy::too_many_lines)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn run_kind_phase(
    shared_file: &std::sync::Arc<std::fs::File>,
    schedule: &[(usize, u64, usize)],
    kind: u8,
    elements_per_blob: usize,
    compression: Compression,
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
) -> Result<PhaseStats> {
    use crate::reorder_buffer::ReorderBuffer;

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

    type PhaseResult = std::result::Result<WorkerOutput, String>;
    // Reorder buffer depth: enough to absorb decode-thread variance
    // without holding too many decoded payloads in flight.
    let mut reorder: ReorderBuffer<PhaseResult> = ReorderBuffer::with_capacity(32);

    let mut bb = BlockBuilder::with_element_cap(elements_per_blob);
    let mut output: Vec<OwnedBlock> = Vec::new();
    let mut pending: Vec<OwnedBlock> = Vec::with_capacity(FRAME_BATCH);
    let mut blobs: u64 = 0;
    let mut elements: u64 = 0;
    let mut coalesces: u64 = 0;
    let mut worker_full_blobs: u64 = 0;
    let mut central_blobs: u64 = 0;
    let mut write_error: Option<Box<dyn std::error::Error>> = None;
    let mut classify_error: Option<String> = None;

    crate::scan::classify::parallel_classify_phase(
        shared_file,
        schedule,
        None,
        || (),
        |block, _state| -> PhaseResult {
            worker_split_blob(block, kind, elements_per_blob, &compression)
        },
        |seq, r| {
            reorder.push(seq, r);
            while let Some(item) = reorder.pop_ready() {
                if write_error.is_some() {
                    continue;
                }
                match item {
                    Ok(out) => {
                        let WorkerOutput { full_framed, tail } = out;
                        let full_count = full_framed.len() as u64 * elements_per_blob as u64;
                        let tail_n = tail.len();

                        // Write the worker's already-framed full blocks directly.
                        for framed in full_framed {
                            if let Err(e) = writer.write_raw_owned(framed) {
                                write_error = Some(e.into());
                                break;
                            }
                            blobs += 1;
                            worker_full_blobs += 1;
                        }
                        if write_error.is_some() {
                            continue;
                        }

                        // Coalesce check: the trailing slice extends a non-empty
                        // central builder iff it's non-empty and the BB carries
                        // residual elements from a prior input blob's tail.
                        if tail_n > 0 && !bb.is_empty() {
                            coalesces += 1;
                        }

                        let consume_res: std::result::Result<(), String> = (|| {
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
                            Ok(())
                        })();
                        if let Err(e) = consume_res {
                            classify_error.get_or_insert(e);
                            continue;
                        }
                        // Anything in `output` came from a mid-stream cap fire on
                        // the central builder; track for the warning logic, then
                        // accumulate into `pending` for batched parallel framing.
                        central_blobs += output.len() as u64;
                        pending.append(&mut output);
                        while pending.len() >= FRAME_BATCH {
                            let batch: Vec<OwnedBlock> =
                                pending.drain(..FRAME_BATCH).collect();
                            match frame_and_write_batch(batch, compression, writer) {
                                Ok(written) => blobs += written,
                                Err(e) => {
                                    write_error = Some(e);
                                    break;
                                }
                            }
                        }
                        elements += full_count + tail_n;
                    }
                    Err(e) => {
                        classify_error.get_or_insert(e);
                    }
                }
            }
        },
    )?;

    if let Some(e) = write_error {
        return Err(e);
    }
    if let Some(e) = classify_error {
        return Err(e.into());
    }

    // Final flush: emit the residual block (if any) for this kind. Any
    // blocks `output` holds at this point are residuals, not mid-stream
    // flushes, so they don't count toward `central_blobs`.
    flush_local(&mut bb, &mut output).map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    let residual = output.len() as u64;
    pending.append(&mut output);
    if !pending.is_empty() {
        let final_batch = std::mem::take(&mut pending);
        let written = frame_and_write_batch(final_batch, compression, writer)?;
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

/// One worker's body: count matching elements, frame the leading
/// `M - M%cap` elements as full blocks via a per-worker `BlockBuilder`,
/// and ship the trailing `M%cap` elements as `Owned*` data for the merge
/// thread to coalesce across input-blob boundaries.
#[allow(clippy::too_many_lines)]
#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn worker_split_blob(
    block: &crate::PrimitiveBlock,
    kind: u8,
    cap: usize,
    compression: &Compression,
) -> std::result::Result<WorkerOutput, String> {
    let total = match kind {
        KIND_NODE => block
            .elements()
            .filter(|e| matches!(e, Element::DenseNode(_) | Element::Node(_)))
            .count(),
        KIND_WAY => block
            .elements()
            .filter(|e| matches!(e, Element::Way(_)))
            .count(),
        KIND_RELATION => block
            .elements()
            .filter(|e| matches!(e, Element::Relation(_)))
            .count(),
        _ => return Err(format!("invalid kind constant: {kind}")),
    };
    let tail_size = total % cap;
    let full_count = total - tail_size;

    let mut bb = BlockBuilder::with_element_cap(cap);
    let mut output: Vec<OwnedBlock> = Vec::new();
    let mut full_framed: Vec<Vec<u8>> = Vec::new();
    let mut tail: KindPayload = match kind {
        KIND_NODE => KindPayload::Nodes(Vec::with_capacity(tail_size)),
        KIND_WAY => KindPayload::Ways(Vec::with_capacity(tail_size)),
        KIND_RELATION => KindPayload::Relations(Vec::with_capacity(tail_size)),
        _ => KindPayload::empty(kind),
    };

    let mut idx: usize = 0;
    let mut refs_buf: Vec<i64> = Vec::new();
    let mut members_buf: Vec<MemberData<'_>> = Vec::new();
    for element in block.elements() {
        let is_match = matches!(
            (&element, kind),
            (Element::DenseNode(_) | Element::Node(_), KIND_NODE)
                | (Element::Way(_), KIND_WAY)
                | (Element::Relation(_), KIND_RELATION)
        );
        if !is_match {
            continue;
        }

        if idx < full_count {
            // Full-block path: re-encode through the per-worker builder
            // exactly like v1. Frame any output produced by `ensure_*`.
            match (&element, kind) {
                (Element::DenseNode(dn), KIND_NODE) => {
                    ensure_node_capacity_local(&mut bb, &mut output)?;
                    let meta = dense_node_metadata(dn);
                    bb.add_node(
                        dn.id(),
                        dn.decimicro_lat(),
                        dn.decimicro_lon(),
                        dn.tags(),
                        meta.as_ref(),
                    );
                }
                (Element::Node(n), KIND_NODE) => {
                    ensure_node_capacity_local(&mut bb, &mut output)?;
                    let meta = element_metadata(&n.info());
                    bb.add_node(
                        n.id(),
                        n.decimicro_lat(),
                        n.decimicro_lon(),
                        n.tags(),
                        meta.as_ref(),
                    );
                }
                (Element::Way(w), KIND_WAY) => {
                    ensure_way_capacity_local(&mut bb, &mut output)?;
                    refs_buf.clear();
                    refs_buf.extend(w.refs());
                    let meta = element_metadata(&w.info());
                    bb.add_way(w.id(), w.tags(), &refs_buf, meta.as_ref());
                }
                (Element::Relation(r), KIND_RELATION) => {
                    ensure_relation_capacity_local(&mut bb, &mut output)?;
                    members_buf.clear();
                    members_buf.extend(r.members().map(|m| MemberData {
                        id: m.id,
                        role: m.role().unwrap_or(""),
                    }));
                    let meta = element_metadata(&r.info());
                    bb.add_relation(r.id(), r.tags(), &members_buf, meta.as_ref());
                }
                _ => {}
            }
            for owned_block in output.drain(..) {
                full_framed.push(frame_owned(owned_block, compression)?);
            }
        } else {
            // Tail path: own and ship to the merge thread.
            match (&element, &mut tail) {
                (Element::DenseNode(dn), KindPayload::Nodes(v)) => v.push(read_dense_node(dn)),
                (Element::Node(n), KindPayload::Nodes(v)) => v.push(read_node(n)),
                (Element::Way(w), KindPayload::Ways(v)) => v.push(read_way(w)),
                (Element::Relation(r), KindPayload::Relations(v)) => v.push(read_relation(r)),
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
        full_framed.push(frame_owned(owned_block, compression)?);
    }

    Ok(WorkerOutput { full_framed, tail })
}

/// Frame a single `OwnedBlock` to wire bytes. Worker-side helper; uses
/// the thread-local `PIPELINE_SCRATCH` so it's safe to call from any
/// rayon / `parallel_classify_phase` worker.
fn frame_owned(
    owned: OwnedBlock,
    compression: &Compression,
) -> std::result::Result<Vec<u8>, String> {
    let (block_bytes, index, tagdata) = owned;
    let indexdata = index.serialize();
    let blob = frame_blob_pipelined(
        &block_bytes,
        compression,
        Some(indexdata.as_slice()),
        tagdata.as_deref(),
    )
    .map_err(|e| e.to_string())?;
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
) -> std::result::Result<u64, Box<dyn std::error::Error>> {
    use rayon::prelude::*;

    let framed: Vec<std::io::Result<Vec<u8>>> = batch
        .into_par_iter()
        .map(|(block_bytes, index, tagdata)| -> std::io::Result<Vec<u8>> {
            let indexdata = index.serialize();
            let blob = frame_blob_pipelined(
                &block_bytes,
                &compression,
                Some(indexdata.as_slice()),
                tagdata.as_deref(),
            )?;
            Ok(blob.into_vec())
        })
        .collect();

    let mut written: u64 = 0;
    for r in framed {
        let bytes = r?;
        writer.write_raw_owned(bytes)?;
        written += 1;
    }
    Ok(written)
}
