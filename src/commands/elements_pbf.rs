//! PBF-oriented owned element types for decode → re-encode round-trips.
//!
//! Used by sort, merge_pbf, and time_filter for overlap-run and sweep-merge operations.

use crate::block_builder::{BlockBuilder, MemberData, Metadata};
use crate::file_writer::FileWriter;
use crate::writer::PbfWriter;

pub(crate) use super::elements_xml::OwnedMember;
use super::Result;

// ---------------------------------------------------------------------------
// Owned element types — Vec fields are kept (not Box<[T]>) because these are
// transient allocations: decoded, processed, re-encoded per overlap run or
// sweep-merge pass. They are not long-lived.
// ---------------------------------------------------------------------------

pub(crate) struct OwnedMetadata {
    pub(crate) version: i32,
    pub(crate) timestamp: i64,
    pub(crate) changeset: i64,
    pub(crate) uid: i32,
    pub(crate) user: String,
    pub(crate) visible: bool,
}

pub(crate) struct OwnedNode {
    pub(crate) id: i64,
    pub(crate) decimicro_lat: i32,
    pub(crate) decimicro_lon: i32,
    pub(crate) tags: Vec<(String, String)>,
    pub(crate) metadata: Option<OwnedMetadata>,
}

pub(crate) struct OwnedWay {
    pub(crate) id: i64,
    pub(crate) tags: Vec<(String, String)>,
    pub(crate) refs: Vec<i64>,
    pub(crate) metadata: Option<OwnedMetadata>,
}

pub(crate) struct OwnedRelation {
    pub(crate) id: i64,
    pub(crate) tags: Vec<(String, String)>,
    pub(crate) members: Vec<OwnedMember>,
    pub(crate) metadata: Option<OwnedMetadata>,
}

pub(crate) enum OwnedElement {
    Node(OwnedNode),
    Way(OwnedWay),
    Relation(OwnedRelation),
}

// ---------------------------------------------------------------------------
// Ord impls — compare by ID via osm_id_cmp, then version ascending as tiebreaker
// ---------------------------------------------------------------------------

/// Extract version from optional metadata (0 if absent).
fn version_of(meta: &Option<OwnedMetadata>) -> i32 {
    meta.as_ref().map_or(0, |m| m.version)
}

impl PartialEq for OwnedNode {
    fn eq(&self, other: &Self) -> bool { self.id == other.id && version_of(&self.metadata) == version_of(&other.metadata) }
}
impl Eq for OwnedNode {}
impl PartialOrd for OwnedNode {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> { Some(self.cmp(other)) }
}
impl Ord for OwnedNode {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        super::osm_id_cmp(self.id, other.id)
            .then_with(|| version_of(&self.metadata).cmp(&version_of(&other.metadata)))
    }
}

impl PartialEq for OwnedWay {
    fn eq(&self, other: &Self) -> bool { self.id == other.id && version_of(&self.metadata) == version_of(&other.metadata) }
}
impl Eq for OwnedWay {}
impl PartialOrd for OwnedWay {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> { Some(self.cmp(other)) }
}
impl Ord for OwnedWay {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        super::osm_id_cmp(self.id, other.id)
            .then_with(|| version_of(&self.metadata).cmp(&version_of(&other.metadata)))
    }
}

impl PartialEq for OwnedRelation {
    fn eq(&self, other: &Self) -> bool { self.id == other.id && version_of(&self.metadata) == version_of(&other.metadata) }
}
impl Eq for OwnedRelation {}
impl PartialOrd for OwnedRelation {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> { Some(self.cmp(other)) }
}
impl Ord for OwnedRelation {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        super::osm_id_cmp(self.id, other.id)
            .then_with(|| version_of(&self.metadata).cmp(&version_of(&other.metadata)))
    }
}

// ---------------------------------------------------------------------------
// OwnedMetadata conversion
// ---------------------------------------------------------------------------

impl OwnedMetadata {
    pub(crate) fn as_borrowed(&self) -> Metadata<'_> {
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

// ---------------------------------------------------------------------------
// OwnedElement methods
// ---------------------------------------------------------------------------

impl OwnedElement {
    pub(crate) fn metadata(&self) -> Option<&OwnedMetadata> {
        match self {
            Self::Node(n) => n.metadata.as_ref(),
            Self::Way(w) => w.metadata.as_ref(),
            Self::Relation(r) => r.metadata.as_ref(),
        }
    }

    pub(crate) fn visible(&self) -> bool {
        self.metadata().is_none_or(|m| m.visible)
    }
}

// ---------------------------------------------------------------------------
// Tags/members helper methods
// ---------------------------------------------------------------------------

impl OwnedNode {
    pub(crate) fn tags_as_pairs(&self) -> Vec<(&str, &str)> {
        self.tags
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }
}

impl OwnedWay {
    pub(crate) fn tags_as_pairs(&self) -> Vec<(&str, &str)> {
        self.tags
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }
}

impl OwnedRelation {
    pub(crate) fn tags_as_pairs(&self) -> Vec<(&str, &str)> {
        self.tags
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }

    pub(crate) fn members_as_data(&self) -> Vec<MemberData<'_>> {
        self.members
            .iter()
            .map(|m| MemberData {
                id: m.id,
                role: &m.role,
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Shared helper
// ---------------------------------------------------------------------------

pub(crate) fn owned_to_metadata(meta: Option<&OwnedMetadata>) -> Option<Metadata<'_>> {
    meta.map(OwnedMetadata::as_borrowed)
}

// ---------------------------------------------------------------------------
// Read functions — convert parsed elements to owned
// ---------------------------------------------------------------------------

pub(crate) fn read_dense_node(dn: &crate::DenseNode<'_>) -> OwnedNode {
    OwnedNode {
        id: dn.id(),
        decimicro_lat: dn.decimicro_lat(),
        decimicro_lon: dn.decimicro_lon(),
        tags: dn
            .tags()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect(),
        // Dense nodes use -1 as "no metadata" sentinel (Osmosis convention).
        // Non-dense elements normalize this at parse time in WireInfo::parse;
        // dense nodes check it here to avoid adding branches to the hot
        // DenseNodeIter path. See CORRECTNESS.md "Osmosis -1 sentinel".
        metadata: dn.info().and_then(|info| {
            if info.version() == -1 {
                return None;
            }
            Some(OwnedMetadata {
                version: info.version(),
                timestamp: info.milli_timestamp() / 1000,
                changeset: if info.changeset() == -1 { 0 } else { info.changeset() },
                uid: info.uid(),
                user: info.user().ok()?.to_owned(),
                visible: info.visible(),
            })
        }),
    }
}

pub(crate) fn read_node(n: &crate::Node<'_>) -> OwnedNode {
    let info = n.info();
    OwnedNode {
        id: n.id(),
        decimicro_lat: n.decimicro_lat(),
        decimicro_lon: n.decimicro_lon(),
        tags: n
            .tags()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect(),
        metadata: info.version().map(|v| OwnedMetadata {
            version: v,
            timestamp: info.milli_timestamp().unwrap_or(0) / 1000,
            changeset: info.changeset().unwrap_or(0),
            uid: info.uid().unwrap_or(0),
            user: info
                .user()
                .and_then(std::result::Result::ok)
                .unwrap_or("")
                .to_owned(),
            visible: info.visible(),
        }),
    }
}

pub(crate) fn read_way(w: &crate::Way<'_>) -> OwnedWay {
    let info = w.info();
    OwnedWay {
        id: w.id(),
        tags: w
            .tags()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect(),
        refs: w.refs().collect(),
        metadata: info.version().map(|v| OwnedMetadata {
            version: v,
            timestamp: info.milli_timestamp().unwrap_or(0) / 1000,
            changeset: info.changeset().unwrap_or(0),
            uid: info.uid().unwrap_or(0),
            user: info
                .user()
                .and_then(std::result::Result::ok)
                .unwrap_or("")
                .to_owned(),
            visible: info.visible(),
        }),
    }
}

pub(crate) fn read_relation(r: &crate::Relation<'_>) -> OwnedRelation {
    let info = r.info();
    OwnedRelation {
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
        metadata: info.version().map(|v| OwnedMetadata {
            version: v,
            timestamp: info.milli_timestamp().unwrap_or(0) / 1000,
            changeset: info.changeset().unwrap_or(0),
            uid: info.uid().unwrap_or(0),
            user: info
                .user()
                .and_then(std::result::Result::ok)
                .unwrap_or("")
                .to_owned(),
            visible: info.visible(),
        }),
    }
}

// ---------------------------------------------------------------------------
// Write functions — write owned elements to BlockBuilder + PbfWriter
// ---------------------------------------------------------------------------

pub(crate) fn write_single_node(
    node: &OwnedNode,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
) -> Result<()> {
    super::ensure_node_capacity(bb, writer)?;
    let meta = owned_to_metadata(node.metadata.as_ref());
    bb.add_node(
        node.id, node.decimicro_lat, node.decimicro_lon,
        node.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())),
        meta.as_ref(),
    );
    Ok(())
}

pub(crate) fn write_single_way(
    way: &OwnedWay,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
) -> Result<()> {
    super::ensure_way_capacity(bb, writer)?;
    let meta = owned_to_metadata(way.metadata.as_ref());
    bb.add_way(
        way.id,
        way.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())),
        &way.refs,
        meta.as_ref(),
    );
    Ok(())
}

pub(crate) fn write_single_relation(
    rel: &OwnedRelation,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
) -> Result<()> {
    super::ensure_relation_capacity(bb, writer)?;
    let members: Vec<MemberData<'_>> = rel
        .members
        .iter()
        .map(|m| MemberData { id: m.id, role: &m.role })
        .collect();
    let meta = owned_to_metadata(rel.metadata.as_ref());
    bb.add_relation(
        rel.id,
        rel.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())),
        &members,
        meta.as_ref(),
    );
    Ok(())
}
