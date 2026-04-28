//! Filter history PBF to a snapshot at a cutoff timestamp.
//!
//! Keeps, for each object ID, the latest version with `timestamp <= cutoff`.
//! If that latest version is deleted (`visible=false`), the object is omitted.
//!
//! Two paths, picked from the header's `HistoricalInformation` required
//! feature flag:
//! - **History input**: sequential pending-group state machine on a
//!   parallel-decode reader. Version selection needs cross-element peek,
//!   and within-group versions can straddle blob boundaries, which rules
//!   out trivial per-block parallelism.
//! - **Snapshot input**: parallel per-block filter. Each (kind, id) appears
//!   in exactly one block, so blocks are independent. Same pattern as
//!   `tags-filter` single-pass: `for_each_primitive_block_batch` +
//!   `par_iter().map_init(BlockBuilder::new, ...)`.

use std::path::Path;

use crate::owned::{
    OwnedElement, dense_node_metadata, element_metadata, owned_to_metadata, read_dense_node,
    read_node, read_relation, read_way,
};
use super::{
    HeaderOverrides, Result,
    ensure_node_capacity_local, ensure_relation_capacity_local, ensure_way_capacity_local,
    flush_local, require_sorted, warn_locations_on_ways_loss, writer_from_header,
};
use crate::block_builder::{BlockBuilder, MemberData, OwnedBlock};
use crate::reorder_buffer::ReorderBuffer;
use crate::scan::classify::{build_classify_schedule, parallel_classify_phase};
use crate::writer::{Compression, PbfWriter};
use crate::{DenseNode, Element, ElementReader, Node, PrimitiveBlock, Relation, Way};

/// Statistics from a `time-filter` snapshot operation.
pub struct TimeFilterStats {
    pub versions_seen: u64,
    pub versions_before_cutoff: u64,
    pub elements_written: u64,
    pub dropped_deleted: u64,
    pub dropped_no_snapshot_version: u64,
}

impl TimeFilterStats {
    pub fn print_summary(&self) {
        eprintln!(
            "time-filter: {} versions scanned, {} <= cutoff, {} elements written ({} deleted at cutoff, {} without version <= cutoff)",
            self.versions_seen,
            self.versions_before_cutoff,
            self.elements_written,
            self.dropped_deleted,
            self.dropped_no_snapshot_version,
        );
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ElementKind {
    Node,
    Way,
    Relation,
}

struct PendingGroup {
    kind: ElementKind,
    id: i64,
    latest: Option<OwnedElement>,
}


/// Filter history PBF to a snapshot at `cutoff_timestamp` (UNIX seconds).
///
/// Requires sorted input (`Sort.Type_then_ID`) so each object's versions are
/// contiguous and the snapshot can be computed in one streaming pass.
#[hotpath::measure]
pub fn time_filter(
    input: &Path,
    output: &Path,
    cutoff_timestamp: i64,
    compression: Compression,
    direct_io: bool,
    overrides: &HeaderOverrides,
) -> Result<TimeFilterStats> {
    // History path: keep the legacy mallopt rule (no M_ARENA_MAX=2).
    // Snapshot path: opts into M_ARENA_MAX=2 internally, post-migration -
    // see the in-function comment in time_filter_snapshot.
    let reader = ElementReader::open(input, direct_io)?;
    require_sorted(reader.header(), input, "Input history PBF")?;
    warn_locations_on_ways_loss(reader.header());

    let is_history = reader.header().has_historical_information();
    let mut writer = writer_from_header(output, compression, reader.header(), true, overrides, |hb| {
        hb.replication_timestamp(cutoff_timestamp)
    }, direct_io, false)?;

    let stats = if is_history {
        time_filter_history(reader, &mut writer, cutoff_timestamp)?
    } else {
        // Drop the reader before the snapshot path - the migrated
        // snapshot path opens the input via HeaderWalker through
        // build_classify_schedule, not ElementReader, so holding the
        // reader open here would just keep an extra fd around.
        drop(reader);
        time_filter_snapshot(input, &mut writer, cutoff_timestamp, compression)?
    };

    writer.flush()?;
    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("timefilter_versions_seen", stats.versions_seen as i64);
        crate::debug::emit_counter("timefilter_versions_before_cutoff", stats.versions_before_cutoff as i64);
        crate::debug::emit_counter("timefilter_elements_written", stats.elements_written as i64);
        crate::debug::emit_counter("timefilter_dropped_deleted", stats.dropped_deleted as i64);
        crate::debug::emit_counter("timefilter_dropped_no_snapshot_version", stats.dropped_no_snapshot_version as i64);
        crate::debug::emit_counter("timefilter_is_history_path", i64::from(is_history));
    }
    Ok(stats)
}

// ---------------------------------------------------------------------------
// History path: pending-group state machine on a parallel-decode reader.
// ---------------------------------------------------------------------------

#[hotpath::measure]
fn time_filter_history(
    reader: ElementReader<crate::file_reader::FileReader>,
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    cutoff_timestamp: i64,
) -> Result<TimeFilterStats> {
    let mut bb = BlockBuilder::new();
    let mut stats = TimeFilterStats {
        versions_seen: 0,
        versions_before_cutoff: 0,
        elements_written: 0,
        dropped_deleted: 0,
        dropped_no_snapshot_version: 0,
    };
    let mut pending: Option<PendingGroup> = None;
    let mut flush_error: Result<()> = Ok(());

    crate::debug::emit_marker("TIMEFILTER_HISTORY_START");
    // Parallel decode via the 3-stage pipelined reader (IO -> rayon decode ->
    // reorder). Element order across blocks is preserved by the reorder
    // stage, which is load-bearing: the pending-group state machine below
    // depends on sorted traversal to know when a group ends. Re-encode and
    // group selection run sequentially on this thread.
    //
    // Hot-path timing lives in `#[cfg_attr(feature = "hotpath",
    // hotpath::measure)]` on the inner helpers - run with `--hotpath` for a
    // per-function breakdown. Release builds pay zero cost for that
    // attribute. Don't re-add per-element Instant::now() here: Japan scale
    // is 344 M elements and the time-source overhead alone doubled wall in
    // an earlier iteration of this instrumentation.
    reader.for_each_pipelined(|element| {
        if flush_error.is_err() {
            return;
        }
        let (kind, id, timestamp) = element_identity_and_timestamp(&element);
        stats.versions_seen += 1;

        let group_changed = pending
            .as_ref()
            .is_some_and(|g| g.kind != kind || g.id != id);
        if group_changed && let Some(group) = pending.take() {
            if let Err(e) = flush_group(group, &mut bb, writer, &mut stats) {
                flush_error = Err(e);
                return;
            }
        }

        if pending.is_none() {
            pending = Some(PendingGroup {
                kind,
                id,
                latest: None,
            });
        }

        // Unconditional overwrite on each matching version is correct
        // under the OSM history-file convention: versions within a
        // `(kind, id)` group arrive in ascending-version order (the
        // same convention PBF history files ship with). The "latest
        // element whose timestamp <= cutoff" is therefore the last one
        // we see during the forward scan. A malformed history file
        // with out-of-order versions would violate this, but that is
        // out of spec.
        if timestamp <= cutoff_timestamp {
            stats.versions_before_cutoff += 1;
            if let Some(group) = pending.as_mut() {
                group.latest = Some(clone_owned_element(&element));
            }
        }
    })?;
    flush_error?;

    if let Some(group) = pending.take() {
        flush_group(group, &mut bb, writer, &mut stats)?;
    }
    if let Some((bytes, index, tagdata)) = bb.take_owned()? {
        writer.write_primitive_block_owned(bytes, index, tagdata.as_deref())?;
    }
    crate::debug::emit_marker("TIMEFILTER_HISTORY_END");
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Snapshot path: parallel per-block filter.
// ---------------------------------------------------------------------------
//
// For non-history input each (kind, id) appears exactly once in the input,
// so blocks are independent: no cross-block group straddling, no version
// selection, no clone-into-OwnedElement on the consumer thread. Each worker
// iterates its block, drops elements whose timestamp > cutoff or whose
// visible=false (rare in snapshot inputs but the field can be set
// explicitly), and writes surviving elements straight into a local
// BlockBuilder via reference. Consumer drains batch results in order.

/// Snapshot path migrated 2026-04-28 from the
/// `for_each_primitive_block_batch_budgeted` + `par_iter().map(thread_local
/// BB)` + drain shape to `parallel_classify_phase` + `ReorderBuffer`. The
/// pre-migration shape hit a structural ~28 GB anon ceiling at planet (five
/// SIGKILL'd attempts, last three with full instrumentation) because the
/// `par_iter().collect()` step materialises the full batch's
/// `Vec<OwnedBlockTriple>` results before draining, and the upstream
/// pipeline keeps stuffing the next batch's PrimitiveBlocks in flight
/// under the cross-thread retention pattern (`pipeline.rs:66-89`).
/// Allocator knobs (`malloc_trim` per batch, `M_MMAP_THRESHOLD=64K`,
/// `decode_ahead=8`) all hit the same ceiling: mallinfo2 showed bytes
/// shifting between `arena` and `hblkhd` but the total live set was
/// genuine working set, not retention. The migration mirrors the
/// `cat --clean` (`b347c0a`, 28.9 GB -> 750 MB) and `check --ids`
/// (`516129e`, 29.2 GB -> 504 MB) precedents.
#[hotpath::measure]
fn time_filter_snapshot(
    input: &Path,
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    cutoff_timestamp: i64,
    compression: Compression,
) -> Result<TimeFilterStats> {
    // Cap glibc arenas to prevent cross-thread alloc/free fragmentation
    // in the per-blob worker pool. Same precedent as cat --clean and
    // check --refs / verify_ids. Post-migration, each blob's BlockBuilder
    // alloc/free cycle is confined to a single worker thread, so the
    // pattern that regressed iter-3 (cross-blob scratch reuse defeated
    // by arena capping) doesn't apply: K=2 measured -69 % wall and
    // -24 % anon on the pre-migration code, but that pre-migration code
    // had thread-local BlockBuilder state surviving across blobs on the
    // same rayon worker. parallel_classify_phase has no such state.
    #[cfg(target_os = "linux")]
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 2);
    }

    let mut stats = TimeFilterStats {
        versions_seen: 0,
        versions_before_cutoff: 0,
        elements_written: 0,
        dropped_deleted: 0,
        dropped_no_snapshot_version: 0,
    };

    crate::debug::emit_marker("TIMEFILTER_SNAPSHOT_START");
    crate::debug::emit_mallinfo2("timefilter_snapshot_start_mallinfo");

    let (schedule, shared_file) = build_classify_schedule(input, None)?;

    if schedule.is_empty() {
        crate::debug::emit_marker("TIMEFILTER_SNAPSHOT_END");
        return Ok(stats);
    }

    type PhaseResult =
        std::result::Result<(Vec<Vec<u8>>, TimeFilterStats), String>;
    let mut reorder: ReorderBuffer<PhaseResult> = ReorderBuffer::with_capacity(64);
    let mut write_error: Option<Box<dyn std::error::Error>> = None;
    let mut classify_error: Option<String> = None;

    // BlockBuilder contains `Rc<str>` (string interning) which is not
    // Send, so it can't ride the `S: Send` worker-state slot. Per-blob
    // alloc inside the closure is cheap (BlockBuilder::new is just a
    // few empty Vec/HashMap initialisers; no heap reservation until
    // elements are added). Same pattern as cat --clean's run_kind_phase.
    parallel_classify_phase(
        &shared_file,
        &schedule,
        None,
        || (),
        |block, _state| -> PhaseResult {
            let mut bb = BlockBuilder::new();
            let mut output: Vec<OwnedBlock> = Vec::new();
            let block_stats =
                filter_block_snapshot(block, cutoff_timestamp, &mut bb, &mut output)?;
            flush_local(&mut bb, &mut output)?;

            let mut framed: Vec<Vec<u8>> = Vec::with_capacity(output.len());
            for (block_bytes, index, tagdata) in output {
                let indexdata = index.serialize();
                let blob = crate::writer::frame_blob_pipelined(
                    &block_bytes,
                    &compression,
                    Some(indexdata.as_slice()),
                    tagdata.as_deref(),
                )
                .map_err(|e| e.to_string())?;
                framed.push(blob.into_vec());
            }
            Ok((framed, block_stats))
        },
        |seq, r| {
            // Always queue the result so the next-seq invariant holds;
            // we can't drop a slot mid-phase without breaking the
            // reorder buffer's contiguous-prefix expectation.
            reorder.push(seq, r);
            // Drain everything ready from the front. Once we hit a
            // hole (next seq not yet delivered), stop and wait for the
            // next merge call. Errors are captured into local Option
            // slots and propagated after parallel_classify_phase
            // returns; further ready items are still drained so the
            // buffer doesn't grow.
            while let Some(r) = reorder.pop_ready() {
                match r {
                    Ok((framed, block_stats)) => {
                        if write_error.is_some() {
                            continue;
                        }
                        stats.versions_seen += block_stats.versions_seen;
                        stats.versions_before_cutoff +=
                            block_stats.versions_before_cutoff;
                        stats.elements_written += block_stats.elements_written;
                        stats.dropped_deleted += block_stats.dropped_deleted;
                        stats.dropped_no_snapshot_version +=
                            block_stats.dropped_no_snapshot_version;
                        for blob in framed {
                            if let Err(e) = writer.write_raw_owned(blob) {
                                write_error = Some(e.into());
                                break;
                            }
                        }
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

    crate::debug::emit_mallinfo2("timefilter_snapshot_end_mallinfo");
    crate::debug::emit_marker("TIMEFILTER_SNAPSHOT_END");
    Ok(stats)
}

/// Snapshot-gate: returns `Some(())` if the element survives both the cutoff
/// timestamp check and the visibility check. Updates `stats` for the drop
/// counters along the way. Returning `None` means the caller should `continue`.
fn snapshot_gate(
    ts: i64,
    visible: bool,
    cutoff_timestamp: i64,
    stats: &mut TimeFilterStats,
) -> Option<()> {
    if ts > cutoff_timestamp {
        stats.dropped_no_snapshot_version += 1;
        return None;
    }
    stats.versions_before_cutoff += 1;
    if !visible {
        stats.dropped_deleted += 1;
        return None;
    }
    Some(())
}

#[hotpath::measure]
fn filter_block_snapshot(
    block: &PrimitiveBlock,
    cutoff_timestamp: i64,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
) -> std::result::Result<TimeFilterStats, String> {
    let mut stats = TimeFilterStats {
        versions_seen: 0,
        versions_before_cutoff: 0,
        elements_written: 0,
        dropped_deleted: 0,
        dropped_no_snapshot_version: 0,
    };
    let mut tags_buf: Vec<(&str, &str)> = Vec::new();
    let mut refs_buf: Vec<i64> = Vec::new();
    let mut members_buf: Vec<MemberData<'_>> = Vec::new();

    for element in block.elements() {
        stats.versions_seen += 1;
        match &element {
            Element::DenseNode(dn) => {
                let Some(()) = snapshot_gate(dense_timestamp(dn), dense_visible(dn), cutoff_timestamp, &mut stats) else { continue; };
                ensure_node_capacity_local(bb, output)?;
                tags_buf.clear();
                tags_buf.extend(dn.tags());
                let meta = dense_node_metadata(dn);
                bb.add_node(
                    dn.id(), dn.decimicro_lat(), dn.decimicro_lon(),
                    tags_buf.iter().copied(), meta.as_ref(),
                );
                stats.elements_written += 1;
            }
            Element::Node(n) => {
                let Some(()) = snapshot_gate(node_timestamp(n), node_visible(n), cutoff_timestamp, &mut stats) else { continue; };
                ensure_node_capacity_local(bb, output)?;
                tags_buf.clear();
                tags_buf.extend(n.tags());
                let meta = element_metadata(&n.info());
                bb.add_node(
                    n.id(), n.decimicro_lat(), n.decimicro_lon(),
                    tags_buf.iter().copied(), meta.as_ref(),
                );
                stats.elements_written += 1;
            }
            Element::Way(w) => {
                let Some(()) = snapshot_gate(way_timestamp(w), way_visible(w), cutoff_timestamp, &mut stats) else { continue; };
                ensure_way_capacity_local(bb, output)?;
                tags_buf.clear();
                tags_buf.extend(w.tags());
                refs_buf.clear();
                refs_buf.extend(w.refs());
                let meta = element_metadata(&w.info());
                bb.add_way(
                    w.id(), tags_buf.iter().copied(), &refs_buf, meta.as_ref(),
                );
                stats.elements_written += 1;
            }
            Element::Relation(r) => {
                let Some(()) = snapshot_gate(relation_timestamp(r), relation_visible(r), cutoff_timestamp, &mut stats) else { continue; };
                ensure_relation_capacity_local(bb, output)?;
                tags_buf.clear();
                tags_buf.extend(r.tags());
                members_buf.clear();
                members_buf.extend(r.members().map(|m| MemberData {
                    id: m.id,
                    role: m.role().unwrap_or(""),
                }));
                let meta = element_metadata(&r.info());
                bb.add_relation(
                    r.id(), tags_buf.iter().copied(), &members_buf, meta.as_ref(),
                );
                stats.elements_written += 1;
            }
        }
    }
    Ok(stats)
}

fn dense_visible(dn: &DenseNode<'_>) -> bool {
    #[allow(clippy::redundant_closure_for_method_calls)]
    dn.info().is_none_or(|i| i.visible())
}

fn node_visible(n: &Node<'_>) -> bool {
    n.info().visible()
}

fn way_visible(w: &Way<'_>) -> bool {
    w.info().visible()
}

fn relation_visible(r: &Relation<'_>) -> bool {
    r.info().visible()
}

#[hotpath::measure]
fn flush_group(
    group: PendingGroup,
    bb: &mut BlockBuilder,
    writer: &mut crate::writer::PbfWriter<crate::file_writer::FileWriter>,
    stats: &mut TimeFilterStats,
) -> Result<()> {
    match group.latest {
        None => {
            stats.dropped_no_snapshot_version += 1;
        }
        Some(owned) if !owned.visible() => {
            stats.dropped_deleted += 1;
        }
        Some(owned) => {
            write_owned_element(bb, writer, owned)?;
            stats.elements_written += 1;
        }
    }
    Ok(())
}

#[hotpath::measure]
fn write_owned_element(
    bb: &mut BlockBuilder,
    writer: &mut crate::writer::PbfWriter<crate::file_writer::FileWriter>,
    elem: OwnedElement,
) -> Result<()> {
    match elem {
        OwnedElement::Node(n) => {
            if !bb.can_add_node()
                && let Some((bytes, index, tagdata)) = bb.take_owned()?
            {
                writer.write_primitive_block_owned(bytes, index, tagdata.as_deref())?;
            }
            let meta = owned_to_metadata(n.metadata.as_ref());
            bb.add_node(n.id, n.decimicro_lat, n.decimicro_lon, n.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())), meta.as_ref());
        }
        OwnedElement::Way(w) => {
            if !bb.can_add_way()
                && let Some((bytes, index, tagdata)) = bb.take_owned()?
            {
                writer.write_primitive_block_owned(bytes, index, tagdata.as_deref())?;
            }
            let meta = owned_to_metadata(w.metadata.as_ref());
            bb.add_way(w.id, w.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())), &w.refs, meta.as_ref());
        }
        OwnedElement::Relation(r) => {
            if !bb.can_add_relation()
                && let Some((bytes, index, tagdata)) = bb.take_owned()?
            {
                writer.write_primitive_block_owned(bytes, index, tagdata.as_deref())?;
            }
            let meta = owned_to_metadata(r.metadata.as_ref());
            let members: Vec<MemberData<'_>> = r.members.iter().map(|m| MemberData { id: m.id, role: &m.role }).collect();
            bb.add_relation(r.id, r.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())), &members, meta.as_ref());
        }
    }
    Ok(())
}

fn element_identity_and_timestamp(element: &Element<'_>) -> (ElementKind, i64, i64) {
    match element {
        Element::DenseNode(dn) => (ElementKind::Node, dn.id(), dense_timestamp(dn)),
        Element::Node(n) => (ElementKind::Node, n.id(), node_timestamp(n)),
        Element::Way(w) => (ElementKind::Way, w.id(), way_timestamp(w)),
        Element::Relation(r) => (ElementKind::Relation, r.id(), relation_timestamp(r)),
    }
}

fn dense_timestamp(dn: &DenseNode<'_>) -> i64 {
    dn.info().map_or(0, |i| i.milli_timestamp() / 1000)
}

fn node_timestamp(n: &Node<'_>) -> i64 {
    n.info().milli_timestamp().unwrap_or(0) / 1000
}

fn way_timestamp(w: &Way<'_>) -> i64 {
    w.info().milli_timestamp().unwrap_or(0) / 1000
}

fn relation_timestamp(r: &Relation<'_>) -> i64 {
    r.info().milli_timestamp().unwrap_or(0) / 1000
}

#[hotpath::measure]
fn clone_owned_element(element: &Element<'_>) -> OwnedElement {
    match element {
        Element::DenseNode(dn) => OwnedElement::Node(read_dense_node(dn)),
        Element::Node(n) => OwnedElement::Node(read_node(n)),
        Element::Way(w) => OwnedElement::Way(read_way(w)),
        Element::Relation(r) => OwnedElement::Relation(read_relation(r)),
    }
}
