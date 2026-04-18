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

use rayon::prelude::*;

use super::elements_pbf::{
    OwnedElement, owned_to_metadata, read_dense_node, read_node, read_way, read_relation,
};
use super::{
    BATCH_SIZE, HeaderOverrides, Result, dense_node_metadata, drain_batch_results,
    element_metadata, ensure_node_capacity_local, ensure_relation_capacity_local,
    ensure_way_capacity_local, flush_local, for_each_primitive_block_batch, require_sorted,
    warn_locations_on_ways_loss, writer_from_header,
};
use crate::block_builder::{BlockBuilder, MemberData};
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
        time_filter_snapshot(reader, &mut writer, cutoff_timestamp)?
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

#[cfg_attr(feature = "hotpath", hotpath::measure)]
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

#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn time_filter_snapshot(
    reader: ElementReader<crate::file_reader::FileReader>,
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    cutoff_timestamp: i64,
) -> Result<TimeFilterStats> {
    let mut stats = TimeFilterStats {
        versions_seen: 0,
        versions_before_cutoff: 0,
        elements_written: 0,
        dropped_deleted: 0,
        dropped_no_snapshot_version: 0,
    };

    crate::debug::emit_marker("TIMEFILTER_SNAPSHOT_START");
    for_each_primitive_block_batch(reader.into_blocks_pipelined(), BATCH_SIZE, |batch| {
        process_snapshot_batch(batch, cutoff_timestamp, writer, &mut stats)
    })?;
    crate::debug::emit_marker("TIMEFILTER_SNAPSHOT_END");
    Ok(stats)
}

fn process_snapshot_batch(
    batch: &[PrimitiveBlock],
    cutoff_timestamp: i64,
    writer: &mut PbfWriter<crate::file_writer::FileWriter>,
    stats: &mut TimeFilterStats,
) -> Result<()> {
    type BatchResult = std::result::Result<(Vec<OwnedBlockTriple>, TimeFilterStats), String>;
    let results: Vec<BatchResult> = batch
        .par_iter()
        .map_init(
            BlockBuilder::new,
            |bb, block| {
                let mut output: Vec<OwnedBlockTriple> = Vec::new();
                let block_stats =
                    filter_block_snapshot(block, cutoff_timestamp, bb, &mut output)?;
                flush_local(bb, &mut output)?;
                Ok((output, block_stats))
            },
        )
        .collect();

    drain_batch_results(results, writer, |s: TimeFilterStats| {
        stats.versions_seen += s.versions_seen;
        stats.versions_before_cutoff += s.versions_before_cutoff;
        stats.elements_written += s.elements_written;
        stats.dropped_deleted += s.dropped_deleted;
        stats.dropped_no_snapshot_version += s.dropped_no_snapshot_version;
    })?;
    Ok(())
}

type OwnedBlockTriple = crate::block_builder::OwnedBlock;

#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn filter_block_snapshot(
    block: &PrimitiveBlock,
    cutoff_timestamp: i64,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlockTriple>,
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
                let ts = dense_timestamp(dn);
                if ts > cutoff_timestamp {
                    stats.dropped_no_snapshot_version += 1;
                    continue;
                }
                stats.versions_before_cutoff += 1;
                if !dense_visible(dn) {
                    stats.dropped_deleted += 1;
                    continue;
                }
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
                let ts = node_timestamp(n);
                if ts > cutoff_timestamp {
                    stats.dropped_no_snapshot_version += 1;
                    continue;
                }
                stats.versions_before_cutoff += 1;
                if !node_visible(n) {
                    stats.dropped_deleted += 1;
                    continue;
                }
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
                let ts = way_timestamp(w);
                if ts > cutoff_timestamp {
                    stats.dropped_no_snapshot_version += 1;
                    continue;
                }
                stats.versions_before_cutoff += 1;
                if !way_visible(w) {
                    stats.dropped_deleted += 1;
                    continue;
                }
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
                let ts = relation_timestamp(r);
                if ts > cutoff_timestamp {
                    stats.dropped_no_snapshot_version += 1;
                    continue;
                }
                stats.versions_before_cutoff += 1;
                if !relation_visible(r) {
                    stats.dropped_deleted += 1;
                    continue;
                }
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

#[cfg_attr(feature = "hotpath", hotpath::measure)]
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

#[cfg_attr(feature = "hotpath", hotpath::measure)]
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

#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn clone_owned_element(element: &Element<'_>) -> OwnedElement {
    match element {
        Element::DenseNode(dn) => OwnedElement::Node(read_dense_node(dn)),
        Element::Node(n) => OwnedElement::Node(read_node(n)),
        Element::Way(w) => OwnedElement::Way(read_way(w)),
        Element::Relation(r) => OwnedElement::Relation(read_relation(r)),
    }
}
