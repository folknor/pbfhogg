//! Filter history PBF to a snapshot at a cutoff timestamp.
//!
//! Keeps, for each object ID, the latest version with `timestamp <= cutoff`.
//! If that latest version is deleted (`visible=false`), the object is omitted.

use std::path::Path;

use super::{Result, require_sorted, warn_locations_on_ways_loss, writer_from_header};
use crate::block_builder::{BlockBuilder, MemberData, Metadata};
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

enum OwnedElement {
    Node(OwnedNode),
    Way(OwnedWay),
    Relation(OwnedRelation),
}

struct OwnedNode {
    id: i64,
    lat: i32,
    lon: i32,
    tags: Vec<(String, String)>,
    metadata: Option<OwnedMetadata>,
}

struct OwnedWay {
    id: i64,
    tags: Vec<(String, String)>,
    refs: Vec<i64>,
    metadata: Option<OwnedMetadata>,
}

struct OwnedRelation {
    id: i64,
    tags: Vec<(String, String)>,
    members: Vec<OwnedMember>,
    metadata: Option<OwnedMetadata>,
}

struct OwnedMember {
    id: crate::MemberId,
    role: String,
}

struct OwnedMetadata {
    version: i32,
    timestamp: i64,
    changeset: i64,
    uid: i32,
    user: String,
    visible: bool,
}

impl OwnedMetadata {
    fn as_borrowed(&self) -> Metadata<'_> {
        Metadata {
            version: self.version,
            timestamp: self.timestamp,
            changeset: self.changeset,
            uid: self.uid,
            user: &self.user,
            visible: self.visible,
        }
    }
}

impl OwnedElement {
    fn metadata(&self) -> Option<&OwnedMetadata> {
        match self {
            Self::Node(n) => n.metadata.as_ref(),
            Self::Way(w) => w.metadata.as_ref(),
            Self::Relation(r) => r.metadata.as_ref(),
        }
    }

    fn visible(&self) -> bool {
        self.metadata().is_none_or(|m| m.visible)
    }
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
) -> Result<TimeFilterStats> {
    let reader = ElementReader::open(input, direct_io)?;
    require_sorted(reader.header(), input, "Input history PBF")?;
    warn_locations_on_ways_loss(reader.header());

    let mut writer = writer_from_header(output, compression, reader.header(), true, |hb| {
        hb.replication_timestamp(cutoff_timestamp)
    })?;
    let mut bb = BlockBuilder::new();
    let mut stats = TimeFilterStats {
        versions_seen: 0,
        versions_before_cutoff: 0,
        elements_written: 0,
        dropped_deleted: 0,
        dropped_no_snapshot_version: 0,
    };
    let mut pending: Option<PendingGroup> = None;

    reader.for_each(|element| {
        let (kind, id, timestamp) = element_identity_and_timestamp(&element);
        stats.versions_seen += 1;

        let group_changed = pending
            .as_ref()
            .is_some_and(|g| g.kind != kind || g.id != id);
        if group_changed && let Some(group) = pending.take() {
            flush_group(group, &mut bb, &mut writer, &mut stats).expect("flush pending group");
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

    if let Some(group) = pending.take() {
        flush_group(group, &mut bb, &mut writer, &mut stats)?;
    }
    if let Some(bytes) = bb.take()? {
        writer.write_primitive_block(bytes)?;
    }
    writer.flush()?;
    Ok(stats)
}

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

fn write_owned_element(
    bb: &mut BlockBuilder,
    writer: &mut crate::writer::PbfWriter<crate::file_writer::FileWriter>,
    elem: OwnedElement,
) -> Result<()> {
    match elem {
        OwnedElement::Node(n) => {
            if !bb.can_add_node()
                && let Some(bytes) = bb.take()?
            {
                writer.write_primitive_block(bytes)?;
            }
            let meta = n.metadata.as_ref().map(OwnedMetadata::as_borrowed);
            bb.add_node(n.id, n.lat, n.lon, &n.tags_as_pairs(), meta.as_ref());
        }
        OwnedElement::Way(w) => {
            if !bb.can_add_way()
                && let Some(bytes) = bb.take()?
            {
                writer.write_primitive_block(bytes)?;
            }
            let meta = w.metadata.as_ref().map(OwnedMetadata::as_borrowed);
            bb.add_way(w.id, &w.tags_as_pairs(), &w.refs, meta.as_ref());
        }
        OwnedElement::Relation(r) => {
            if !bb.can_add_relation()
                && let Some(bytes) = bb.take()?
            {
                writer.write_primitive_block(bytes)?;
            }
            let meta = r.metadata.as_ref().map(OwnedMetadata::as_borrowed);
            let members = r.members_as_data();
            bb.add_relation(r.id, &r.tags_as_pairs(), &members, meta.as_ref());
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

fn clone_owned_element(element: &Element<'_>) -> OwnedElement {
    match element {
        Element::DenseNode(dn) => OwnedElement::Node(OwnedNode {
            id: dn.id(),
            lat: dn.decimicro_lat(),
            lon: dn.decimicro_lon(),
            tags: dn
                .tags()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect(),
            metadata: super::dense_node_metadata(dn).map(owned_metadata),
        }),
        Element::Node(n) => OwnedElement::Node(OwnedNode {
            id: n.id(),
            lat: n.decimicro_lat(),
            lon: n.decimicro_lon(),
            tags: n
                .tags()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect(),
            metadata: super::element_metadata(&n.info()).map(owned_metadata),
        }),
        Element::Way(w) => OwnedElement::Way(OwnedWay {
            id: w.id(),
            tags: w
                .tags()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect(),
            refs: w.refs().collect(),
            metadata: super::element_metadata(&w.info()).map(owned_metadata),
        }),
        Element::Relation(r) => OwnedElement::Relation(OwnedRelation {
            id: r.id(),
            tags: r
                .tags()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect(),
            members: r
                .members()
                .map(|m| OwnedMember {
                    id: m.id,
                    role: m.role().unwrap_or("").to_owned(),
                })
                .collect(),
            metadata: super::element_metadata(&r.info()).map(owned_metadata),
        }),
    }
}

fn owned_metadata(meta: Metadata<'_>) -> OwnedMetadata {
    OwnedMetadata {
        version: meta.version,
        timestamp: meta.timestamp,
        changeset: meta.changeset,
        uid: meta.uid,
        user: meta.user.to_owned(),
        visible: meta.visible,
    }
}

impl OwnedNode {
    fn tags_as_pairs(&self) -> Vec<(&str, &str)> {
        self.tags
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }
}

impl OwnedWay {
    fn tags_as_pairs(&self) -> Vec<(&str, &str)> {
        self.tags
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }
}

impl OwnedRelation {
    fn tags_as_pairs(&self) -> Vec<(&str, &str)> {
        self.tags
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }

    fn members_as_data(&self) -> Vec<MemberData<'_>> {
        self.members
            .iter()
            .map(|m| MemberData {
                id: m.id,
                role: &m.role,
            })
            .collect()
    }
}
