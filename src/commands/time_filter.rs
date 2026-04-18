//! Filter history PBF to a snapshot at a cutoff timestamp.
//!
//! Keeps, for each object ID, the latest version with `timestamp <= cutoff`.
//! If that latest version is deleted (`visible=false`), the object is omitted.

use std::path::Path;

use super::elements_pbf::{
    OwnedElement, owned_to_metadata, read_dense_node, read_node, read_way, read_relation,
};
use super::{HeaderOverrides, Result, require_sorted, warn_locations_on_ways_loss, writer_from_header};
use crate::block_builder::{BlockBuilder, MemberData};
use crate::writer::Compression;
use crate::{DenseNode, Element, ElementReader, Node, Relation, Way};

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

    let mut writer = writer_from_header(output, compression, reader.header(), true, overrides, |hb| {
        hb.replication_timestamp(cutoff_timestamp)
    }, direct_io, false)?;
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

    crate::debug::emit_marker("TIMEFILTER_START");
    // Parallel decode via the 3-stage pipelined reader (IO -> rayon decode ->
    // reorder). Element order across blocks is preserved by the reorder stage,
    // which is load-bearing: the pending-group state machine below depends on
    // sorted traversal to know when a group ends. Re-encode and group
    // selection remain sequential on this thread.
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
            if let Err(e) = flush_group(group, &mut bb, &mut writer, &mut stats) {
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
        flush_group(group, &mut bb, &mut writer, &mut stats)?;
    }
    if let Some((bytes, index, tagdata)) = bb.take_owned()? {
        writer.write_primitive_block_owned(bytes, index, tagdata.as_deref())?;
    }
    writer.flush()?;
    crate::debug::emit_marker("TIMEFILTER_END");
    #[allow(clippy::cast_possible_wrap)]
    {
        crate::debug::emit_counter("timefilter_versions_seen", stats.versions_seen as i64);
        crate::debug::emit_counter("timefilter_versions_before_cutoff", stats.versions_before_cutoff as i64);
        crate::debug::emit_counter("timefilter_elements_written", stats.elements_written as i64);
        crate::debug::emit_counter("timefilter_dropped_deleted", stats.dropped_deleted as i64);
        crate::debug::emit_counter("timefilter_dropped_no_snapshot_version", stats.dropped_no_snapshot_version as i64);
    }
    Ok(stats)
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
