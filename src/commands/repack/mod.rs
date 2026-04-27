//! Re-encode a PBF with a configurable per-blob element cap.
//!
//! Primary consumer: the blob-density measurement matrix in
//! [`reference/blob-density.md`](../../../reference/blob-density.md). That
//! matrix needs same-corpus-different-encoding pairs to control for blob-
//! count effects independent of byte size; `repack` is the only way to
//! produce them.
//!
//! Implementation: parallel three-phase scan (nodes, ways, relations),
//! mirroring `cat --clean`'s per-kind worker shape. Each worker decodes
//! one input blob, re-encodes its elements through a `BlockBuilder`
//! configured with the requested cap, and emits the resulting output
//! blob(s) as framed bytes. Output is streamed in input-seq order via
//! a `ReorderBuffer`, so peak RSS is bounded by the in-flight worker
//! count rather than the total output size.
//!
//! v1 limitation: the cap fires per worker invocation, so output blobs
//! cannot grow beyond the input blob size. To shrink (planet's
//! ~228 k/blob -> 8 k/blob) one input blob produces multiple output
//! blobs and the cap fires correctly; this is the blob-density
//! measurement use case. To grow (8 k/blob -> 64 k/blob) cross-input-
//! blob coalescing would be needed and is deferred to v2.

use std::path::Path;

use super::{
    ensure_node_capacity_local, ensure_relation_capacity_local, ensure_way_capacity_local,
    flush_local, require_indexdata, writer_from_header, HeaderOverrides, Result,
};
use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::owned::{dense_node_metadata, element_metadata};
use crate::writer::{frame_blob_pipelined, Compression, PbfWriter};
use crate::{Element, ElementReader};

const KIND_NODE: u8 = 0;
const KIND_WAY: u8 = 1;
const KIND_RELATION: u8 = 2;

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

/// Re-encode `input` to `output` with a per-blob element cap of
/// `elements_per_blob`. Element semantics are preserved: every element
/// round-trips with its tags, refs, members, metadata, and DenseNodes
/// encoding. Output is type-sorted (nodes, then ways, then relations);
/// the `Sort.Type_then_ID` flag is propagated when the input has it.
#[allow(clippy::too_many_arguments)]
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
    // the per-blob worker pool. Same precedent as `cat --clean`: each blob's
    // alloc/free cycle is confined to a single worker thread, so the
    // cross-blob scratch reuse pattern (which `mallopt` defeats and which
    // `time-filter` relies on) does not apply here.
    #[cfg(target_os = "linux")]
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

    let mut blobs_written: u64 = 0;
    let mut elements_written: u64 = 0;

    crate::debug::emit_marker("REPACK_NODES_START");
    let (b, e) = run_kind_phase(
        &shared_file,
        &node_schedule,
        KIND_NODE,
        elements_per_blob,
        compression,
        &mut writer,
    )?;
    blobs_written += b;
    elements_written += e;
    crate::debug::emit_marker("REPACK_NODES_END");

    crate::debug::emit_marker("REPACK_WAYS_START");
    let (b, e) = run_kind_phase(
        &shared_file,
        &way_schedule,
        KIND_WAY,
        elements_per_blob,
        compression,
        &mut writer,
    )?;
    blobs_written += b;
    elements_written += e;
    crate::debug::emit_marker("REPACK_WAYS_END");

    crate::debug::emit_marker("REPACK_RELATIONS_START");
    let (b, e) = run_kind_phase(
        &shared_file,
        &rel_schedule,
        KIND_RELATION,
        elements_per_blob,
        compression,
        &mut writer,
    )?;
    blobs_written += b;
    elements_written += e;
    crate::debug::emit_marker("REPACK_RELATIONS_END");

    writer.flush()?;

    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("repack_blobs_written", blobs_written as i64);
        crate::debug::emit_counter("repack_elements_written", elements_written as i64);
    }

    Ok(RepackStats {
        blobs_written,
        elements_written,
        elements_per_blob,
    })
}

/// Run one per-kind phase: pread workers decode + re-encode (with the
/// caller's element cap) + frame each input blob's matching elements;
/// main thread streams framed bytes to the writer in seq order via a
/// `ReorderBuffer`.
#[allow(clippy::too_many_lines)]
fn run_kind_phase(
    shared_file: &std::sync::Arc<std::fs::File>,
    schedule: &[(usize, u64, usize)],
    kind: u8,
    elements_per_blob: usize,
    compression: Compression,
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
) -> Result<(u64, u64)> {
    use crate::reorder_buffer::ReorderBuffer;

    if schedule.is_empty() {
        return Ok((0, 0));
    }

    type PhaseResult = std::result::Result<(Vec<Vec<u8>>, u64), String>;
    let mut reorder: ReorderBuffer<PhaseResult> = ReorderBuffer::with_capacity(64);
    let mut total_blobs: u64 = 0;
    let mut total_elements: u64 = 0;
    let mut write_error: Option<Box<dyn std::error::Error>> = None;
    let mut classify_error: Option<String> = None;

    crate::scan::classify::parallel_classify_phase(
        shared_file,
        schedule,
        None,
        || (),
        |block, _state| -> PhaseResult {
            let mut bb = BlockBuilder::with_element_cap(elements_per_blob);
            let mut output: Vec<OwnedBlock> = Vec::new();
            let mut refs_buf: Vec<i64> = Vec::new();
            let mut members_buf: Vec<MemberData<'_>> = Vec::new();
            let mut count: u64 = 0;

            for element in block.elements() {
                match &element {
                    Element::DenseNode(dn) if kind == KIND_NODE => {
                        ensure_node_capacity_local(&mut bb, &mut output)?;
                        let meta = dense_node_metadata(dn);
                        bb.add_node(
                            dn.id(),
                            dn.decimicro_lat(),
                            dn.decimicro_lon(),
                            dn.tags(),
                            meta.as_ref(),
                        );
                        count += 1;
                    }
                    Element::Node(n) if kind == KIND_NODE => {
                        ensure_node_capacity_local(&mut bb, &mut output)?;
                        let meta = element_metadata(&n.info());
                        bb.add_node(
                            n.id(),
                            n.decimicro_lat(),
                            n.decimicro_lon(),
                            n.tags(),
                            meta.as_ref(),
                        );
                        count += 1;
                    }
                    Element::Way(w) if kind == KIND_WAY => {
                        ensure_way_capacity_local(&mut bb, &mut output)?;
                        refs_buf.clear();
                        refs_buf.extend(w.refs());
                        let meta = element_metadata(&w.info());
                        bb.add_way(w.id(), w.tags(), &refs_buf, meta.as_ref());
                        count += 1;
                    }
                    Element::Relation(r) if kind == KIND_RELATION => {
                        ensure_relation_capacity_local(&mut bb, &mut output)?;
                        members_buf.clear();
                        members_buf.extend(r.members().map(|m| MemberData {
                            id: m.id,
                            role: m.role().unwrap_or(""),
                        }));
                        let meta = element_metadata(&r.info());
                        bb.add_relation(r.id(), r.tags(), &members_buf, meta.as_ref());
                        count += 1;
                    }
                    _ => {}
                }
            }
            flush_local(&mut bb, &mut output)?;

            let mut framed: Vec<Vec<u8>> = Vec::with_capacity(output.len());
            for (block_bytes, index, tagdata) in output {
                let indexdata = index.serialize();
                let blob = frame_blob_pipelined(
                    &block_bytes,
                    &compression,
                    Some(indexdata.as_slice()),
                    tagdata.as_deref(),
                )
                .map_err(|e| e.to_string())?;
                framed.push(blob.into_vec());
            }
            Ok((framed, count))
        },
        |seq, r| {
            reorder.push(seq, r);
            while let Some(r) = reorder.pop_ready() {
                match r {
                    Ok((framed, count)) => {
                        if write_error.is_some() {
                            continue;
                        }
                        for blob in framed {
                            if let Err(e) = writer.write_raw_owned(blob) {
                                write_error = Some(e.into());
                                break;
                            }
                            total_blobs += 1;
                        }
                        total_elements += count;
                    }
                    Err(e) => {
                        if classify_error.is_none() {
                            classify_error = Some(e);
                        }
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

    Ok((total_blobs, total_elements))
}
