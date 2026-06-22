//! PBF-oriented owned element types for decode → re-encode round-trips.
//!
//! Used by sort, merge_pbf, and time_filter for overlap-run and sweep-merge operations.

use crate::block_builder::{BlockBuilder, MemberData, Metadata, OwnedBlock, RawMetadata};
use crate::file_writer::FileWriter;
use crate::writer::PbfWriter;

use crate::BoxResult;
pub(crate) use crate::osc::write::OwnedMember;

// ---------------------------------------------------------------------------
// Element type filter (node | way | relation), used by tag_expr matchers and
// command-side type narrowing flags.
// ---------------------------------------------------------------------------

/// Boolean filter for which element types to include.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TypeFilter {
    pub(crate) nodes: bool,
    pub(crate) ways: bool,
    pub(crate) relations: bool,
}

impl TypeFilter {
    /// All types included.
    pub(crate) fn all() -> Self {
        Self {
            nodes: true,
            ways: true,
            relations: true,
        }
    }

    /// Parse a comma-separated type list (e.g. "node,way,relation").
    pub(crate) fn parse(s: &str) -> Self {
        Self {
            nodes: s.split(',').any(|t| t.trim() == "node"),
            ways: s.split(',').any(|t| t.trim() == "way"),
            relations: s.split(',').any(|t| t.trim() == "relation"),
        }
    }

    /// Single type filter, or all types if `None`.
    pub(crate) fn from_single(s: Option<&str>) -> Self {
        match s {
            None => Self::all(),
            Some("node") => Self {
                nodes: true,
                ways: false,
                relations: false,
            },
            Some("way") => Self {
                nodes: false,
                ways: true,
                relations: false,
            },
            Some("relation") => Self {
                nodes: false,
                ways: false,
                relations: true,
            },
            Some(_) => Self {
                nodes: false,
                ways: false,
                relations: false,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Owned element types - Vec fields are kept (not Box<[T]>) because these are
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
// Ord impls - compare by ID via osm_id_cmp, then version ascending as tiebreaker
// ---------------------------------------------------------------------------

/// Extract version from optional metadata (0 if absent).
fn version_of(meta: &Option<OwnedMetadata>) -> i32 {
    meta.as_ref().map_or(0, |m| m.version)
}

impl PartialEq for OwnedNode {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && version_of(&self.metadata) == version_of(&other.metadata)
    }
}
impl Eq for OwnedNode {}
impl PartialOrd for OwnedNode {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for OwnedNode {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        crate::osm_id::osm_id_cmp(self.id, other.id)
            .then_with(|| version_of(&self.metadata).cmp(&version_of(&other.metadata)))
    }
}

impl PartialEq for OwnedWay {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && version_of(&self.metadata) == version_of(&other.metadata)
    }
}
impl Eq for OwnedWay {}
impl PartialOrd for OwnedWay {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for OwnedWay {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        crate::osm_id::osm_id_cmp(self.id, other.id)
            .then_with(|| version_of(&self.metadata).cmp(&version_of(&other.metadata)))
    }
}

impl PartialEq for OwnedRelation {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && version_of(&self.metadata) == version_of(&other.metadata)
    }
}
impl Eq for OwnedRelation {}
impl PartialOrd for OwnedRelation {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for OwnedRelation {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        crate::osm_id::osm_id_cmp(self.id, other.id)
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
// Shared helper
// ---------------------------------------------------------------------------

pub(crate) fn owned_to_metadata(meta: Option<&OwnedMetadata>) -> Option<Metadata<'_>> {
    meta.map(OwnedMetadata::as_borrowed)
}

// ---------------------------------------------------------------------------
// Borrowed-element to Metadata extraction
// ---------------------------------------------------------------------------

/// Map sentinel value -1 to 0 for `dense_node_metadata` and
/// `dense_node_raw_metadata`. The PBF spec uses -1 in DenseInfo
/// fields where the type is plain `i64` rather than `Option`.
#[inline]
fn map_sentinel(value: i64) -> i64 {
    if value == -1 { 0 } else { value }
}

/// Extract [`Metadata`] from an [`Info`](crate::Info) (Node/Way/Relation).
///
/// Returns `None` if the info block has no version. On `user()` error (string
/// table corruption), defaults to empty string.
pub(crate) fn element_metadata<'a>(info: &crate::Info<'a>) -> Option<Metadata<'a>> {
    info.version().map(|v| Metadata {
        version: v,
        timestamp: info.milli_timestamp().unwrap_or(0) / 1000,
        changeset: info.changeset().unwrap_or(0),
        uid: info.uid().unwrap_or(0),
        user: info.user().and_then(std::result::Result::ok).unwrap_or(""),
        visible: info.visible(),
    })
}

/// Extract [`Metadata`] from a [`DenseNode`](crate::DenseNode).
///
/// Returns `None` if the node has no info block. On `user()` error (string
/// table corruption), defaults to empty string - consistent with the
/// Node/Way/Relation path.
pub(crate) fn dense_node_metadata<'a>(dn: &'a crate::DenseNode<'a>) -> Option<Metadata<'a>> {
    dn.info()
        .filter(|info| info.version() != -1)
        .map(|info| Metadata {
            version: info.version(),
            timestamp: info.milli_timestamp() / 1000,
            changeset: map_sentinel(info.changeset()),
            uid: info.uid(),
            user: info.user().unwrap_or(""),
            visible: info.visible(),
        })
}

/// Extract [`RawMetadata`] from an [`Info`](crate::Info), preserving the raw
/// string table index for the user name.
pub(crate) fn element_raw_metadata(info: &crate::Info<'_>) -> Option<RawMetadata> {
    info.version().map(|v| RawMetadata {
        version: v,
        timestamp: info.milli_timestamp().unwrap_or(0) / 1000,
        changeset: info.changeset().unwrap_or(0),
        uid: info.uid().unwrap_or(0),
        user_sid: info.raw_user_sid().unwrap_or(0),
        visible: info.visible(),
    })
}

/// Extract [`RawMetadata`] from a [`DenseNode`](crate::DenseNode), preserving
/// the raw string table index for the user name.
pub(crate) fn dense_node_raw_metadata(dn: &crate::DenseNode<'_>) -> Option<RawMetadata> {
    dn.info()
        .filter(|info| info.version() != -1)
        .map(|info| RawMetadata {
            version: info.version(),
            timestamp: info.milli_timestamp() / 1000,
            changeset: map_sentinel(info.changeset()),
            uid: info.uid(),
            user_sid: info.raw_user_sid(),
            visible: info.visible(),
        })
}

// ---------------------------------------------------------------------------
// Read functions - convert parsed elements to owned
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
                changeset: if info.changeset() == -1 {
                    0
                } else {
                    info.changeset()
                },
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
// Write functions - write owned elements to BlockBuilder + PbfWriter
// ---------------------------------------------------------------------------

pub(crate) fn write_single_node(
    node: &OwnedNode,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
) -> BoxResult<()> {
    crate::commands::ensure_node_capacity(bb, writer)?;
    let meta = owned_to_metadata(node.metadata.as_ref());
    bb.add_node(
        node.id,
        node.decimicro_lat,
        node.decimicro_lon,
        node.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())),
        meta.as_ref(),
    );
    Ok(())
}

pub(crate) fn write_single_way(
    way: &OwnedWay,
    bb: &mut BlockBuilder,
    writer: &mut PbfWriter<FileWriter>,
) -> BoxResult<()> {
    crate::commands::ensure_way_capacity(bb, writer)?;
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
) -> BoxResult<()> {
    crate::commands::ensure_relation_capacity(bb, writer)?;
    let members: Vec<MemberData<'_>> = rel
        .members
        .iter()
        .map(|m| MemberData {
            id: m.id,
            role: &m.role,
        })
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

// Local-output variants (emit into `Vec<OwnedBlock>`) for rayon worker
// threads. Mirror the `write_single_*` functions above but call the
// `ensure_*_capacity_local` / `flush_local` helpers so no writer thread
// is touched from the worker.

pub(crate) fn write_single_node_local(
    node: &OwnedNode,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
) -> std::result::Result<(), String> {
    crate::commands::ensure_node_capacity_local(bb, output)?;
    let meta = owned_to_metadata(node.metadata.as_ref());
    bb.add_node(
        node.id,
        node.decimicro_lat,
        node.decimicro_lon,
        node.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())),
        meta.as_ref(),
    );
    Ok(())
}

pub(crate) fn write_single_way_local(
    way: &OwnedWay,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
) -> std::result::Result<(), String> {
    crate::commands::ensure_way_capacity_local(bb, output)?;
    let meta = owned_to_metadata(way.metadata.as_ref());
    bb.add_way(
        way.id,
        way.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())),
        &way.refs,
        meta.as_ref(),
    );
    Ok(())
}

pub(crate) fn write_single_relation_local(
    rel: &OwnedRelation,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
) -> std::result::Result<(), String> {
    crate::commands::ensure_relation_capacity_local(bb, output)?;
    let members: Vec<MemberData<'_>> = rel
        .members
        .iter()
        .map(|m| MemberData {
            id: m.id,
            role: &m.role,
        })
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
