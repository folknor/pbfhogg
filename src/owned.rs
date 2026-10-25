//! PBF-oriented owned element types for decode → re-encode round-trips.
//!
//! Used by the blob-level merge (sort, cat --dedupe) and time_filter for
//! overlap-run and sweep-merge operations.

use crate::block_builder::{BlockBuilder, MemberData, Metadata, OwnedBlock, RawMetadata};

use crate::error::Result;
pub(crate) use crate::osc::write::OwnedMember;

// ---------------------------------------------------------------------------
// Element type filter (node | way | relation), used by tag_expr matchers and
// command-side type narrowing flags.
// ---------------------------------------------------------------------------

/// Boolean filter for which element types to include.
///
/// This is the parsed form of every command's `--type` argument. It is
/// public so callers parse the user's string exactly once, at their own
/// argument boundary, and hand the library a value that cannot be
/// misspelled: no command option takes an unvalidated type string.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TypeFilter {
    pub nodes: bool,
    pub ways: bool,
    pub relations: bool,
}

impl TypeFilter {
    /// All types included.
    #[must_use]
    pub fn all() -> Self {
        Self {
            nodes: true,
            ways: true,
            relations: true,
        }
    }

    /// No types included.
    #[must_use]
    pub fn none() -> Self {
        Self {
            nodes: false,
            ways: false,
            relations: false,
        }
    }

    /// How many of the three element types this filter admits.
    #[must_use]
    pub fn count(self) -> usize {
        usize::from(self.nodes) + usize::from(self.ways) + usize::from(self.relations)
    }

    /// Whether this filter admits the given element kind.
    #[must_use]
    pub fn admits(self, kind: crate::ElementKind) -> bool {
        match kind {
            crate::ElementKind::Node => self.nodes,
            crate::ElementKind::Way => self.ways,
            crate::ElementKind::Relation => self.relations,
        }
    }

    /// The filter's sole element kind, when it admits exactly one.
    /// Drives the blob-level classify schedule.
    pub(crate) fn single_kind(self) -> Option<crate::ElementKind> {
        match (self.nodes, self.ways, self.relations) {
            (true, false, false) => Some(crate::ElementKind::Node),
            (false, true, false) => Some(crate::ElementKind::Way),
            (false, false, true) => Some(crate::ElementKind::Relation),
            _ => None,
        }
    }

    /// The given filter, or all types if `None`.
    #[must_use]
    pub fn or_all(filter: Option<Self>) -> Self {
        filter.unwrap_or_else(Self::all)
    }
}

/// Parse error for [`TypeFilter`] strings.
#[derive(Debug, Clone)]
pub struct ParseTypeFilterError(String);

impl std::fmt::Display for ParseTypeFilterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseTypeFilterError {}

impl std::str::FromStr for TypeFilter {
    type Err = ParseTypeFilterError;

    /// Parse a comma-separated element-type list. Unknown types and an
    /// empty list are errors - a `--type` the user misspelled must not
    /// quietly become "match nothing".
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        if s.is_empty() {
            return Err(ParseTypeFilterError(
                "type filter must not be empty".to_string(),
            ));
        }
        let mut filter = Self::none();
        for kind in s.split(',') {
            match kind.trim() {
                "node" => filter.nodes = true,
                "way" => filter.ways = true,
                "relation" => filter.relations = true,
                other => {
                    return Err(ParseTypeFilterError(format!(
                        "invalid element type '{other}' (expected node, way, or relation)"
                    )));
                }
            }
        }
        Ok(filter)
    }
}

impl std::fmt::Display for TypeFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut first = true;
        for (on, name) in [
            (self.nodes, "node"),
            (self.ways, "way"),
            (self.relations, "relation"),
        ] {
            if on {
                if !first {
                    f.write_str(",")?;
                }
                f.write_str(name)?;
                first = false;
            }
        }
        Ok(())
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
    /// Inline node coordinates (decimicro lat/lon), one per ref, for
    /// LocationsOnWays preservation. Empty when the source way carried no
    /// inline coordinates (the common case: input header lacks the
    /// `LocationsOnWays` feature). `read_way` always leaves this empty;
    /// only `read_way_with_locations` populates it, so existing consumers
    /// (sort, cat dedupe, degrade, time_filter) that go through `read_way`
    /// keep re-encoding via the plain `add_way` path unchanged. The
    /// `write_single_way*` helpers switch to `add_way_with_locations` only
    /// when this is non-empty and its length matches `refs`.
    pub(crate) locations: Vec<(i32, i32)>,
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
    /// Own a borrowed [`Metadata`]: the one field mapping the owned
    /// conversions share with [`element_metadata`] and
    /// [`dense_node_metadata`], so an owned element and a borrowed re-encode
    /// of the same input write the same metadata.
    fn from_borrowed(meta: Metadata<'_>) -> Self {
        Self {
            version: meta.version,
            timestamp: meta.timestamp,
            changeset: meta.changeset,
            uid: meta.uid,
            user: meta.user.to_owned(),
            visible: meta.visible,
        }
    }

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

/// Extract [`Metadata`] from the [`Info`](crate::Info) of the Node, Way or
/// Relation `kind` `id`, for re-encoding.
///
/// Returns `None` if the info block has no version: the element is then
/// re-encoded without metadata, whatever other fields its `Info` carries.
/// Absent timestamp, changeset and uid become 0, the block builder's
/// normalisation for a field it cannot leave out of a written `Info`.
///
/// A user name that is written is resolved, never substituted: a user string
/// index past the block's string table, or negative, is an error naming the
/// element and the index, not an empty user. Only the metadata this returns
/// is resolved - a versionless `Info` is dropped whole, so its user is never
/// looked up - and the refusal stands even where the caller later clears the
/// user (`cat --clean user`), since the input is corrupt either way.
pub(crate) fn element_metadata<'a>(
    kind: crate::ElementKind,
    id: i64,
    info: &crate::Info<'a>,
) -> Result<Option<Metadata<'a>>> {
    let Some(version) = info.version() else {
        return Ok(None);
    };
    let user = match info.user() {
        None => "",
        Some(user) => user.map_err(|e| bad_user(e, kind, id, info.raw_user_sid()))?,
    };
    Ok(Some(Metadata {
        version,
        timestamp: info.milli_timestamp().unwrap_or(0) / 1000,
        changeset: info.changeset().unwrap_or(0),
        uid: info.uid().unwrap_or(0),
        user,
        visible: info.visible(),
    }))
}

/// Extract [`Metadata`] from a [`DenseNode`](crate::DenseNode), for
/// re-encoding.
///
/// Returns `None` if the node has no info block, or carries the Osmosis -1
/// "absent" version. A version of 0 keeps its metadata. The user name is
/// resolved as in [`element_metadata`]: a bad user string index is an error,
/// not an empty user.
pub(crate) fn dense_node_metadata<'a>(
    dn: &'a crate::DenseNode<'a>,
) -> Result<Option<Metadata<'a>>> {
    let Some(info) = dn.info().filter(|info| info.version() != -1) else {
        return Ok(None);
    };
    let user = info.user().map_err(|e| {
        bad_user(
            e,
            crate::ElementKind::Node,
            dn.id(),
            Some(info.raw_user_sid()),
        )
    })?;
    Ok(Some(Metadata {
        version: info.version(),
        timestamp: info.milli_timestamp() / 1000,
        changeset: map_sentinel(info.changeset()),
        uid: info.uid(),
        user,
        visible: info.visible(),
    }))
}

/// Name the element and the string index on a failed user lookup.
#[cold]
#[inline(never)]
fn bad_user(
    err: crate::Error,
    kind: crate::ElementKind,
    id: i64,
    sid: Option<i32>,
) -> crate::Error {
    let sid = sid.map_or_else(String::new, |sid| format!(" string index {sid}"));
    err.context(format!("{} {id}: user name{sid}", kind.as_str()))
}

/// Resolve every member of `r` into `buf`, which is cleared first. A role
/// string index past the block's string table, or negative, is an error
/// naming the relation and the member's position, not an empty role. On an
/// error `buf` holds the members before the bad one; the caller propagates.
pub(crate) fn relation_members_into<'a>(
    r: &crate::Relation<'a>,
    buf: &mut Vec<MemberData<'a>>,
) -> Result<()> {
    buf.clear();
    for (position, m) in r.members().enumerate() {
        let role = m
            .role()
            .map_err(|e| bad_role(e, r.id(), position, m.role_sid))?;
        buf.push(MemberData { id: m.id, role });
    }
    Ok(())
}

/// Name the relation, the member position and the string index on a failed
/// role lookup.
#[cold]
#[inline(never)]
fn bad_role(err: crate::Error, id: i64, position: usize, sid: i32) -> crate::Error {
    err.context(format!(
        "relation {id} member {position}: role string index {sid}"
    ))
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

// The metadata of every owned conversion comes from `element_metadata` /
// `dense_node_metadata`, so an owned re-encode and a borrowed one write the
// same metadata and refuse the same bad user name. Tags still go through the
// infallible tag iterators: see CORRECTNESS.md "Tag iterators end early on an
// out-of-range string index".

pub(crate) fn read_dense_node(dn: &crate::DenseNode<'_>) -> Result<OwnedNode> {
    Ok(OwnedNode {
        id: dn.id(),
        decimicro_lat: dn.decimicro_lat(),
        decimicro_lon: dn.decimicro_lon(),
        tags: dn
            .tags()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect(),
        // Dense nodes use -1 as "no metadata" sentinel (Osmosis convention).
        // Non-dense elements normalize this at parse time in WireInfo::parse;
        // dense nodes check it in `dense_node_metadata` to avoid adding
        // branches to the hot DenseNodeIter path. See CORRECTNESS.md
        // "Osmosis -1 sentinel".
        metadata: dense_node_metadata(dn)?.map(OwnedMetadata::from_borrowed),
    })
}

pub(crate) fn read_node(n: &crate::Node<'_>) -> Result<OwnedNode> {
    Ok(OwnedNode {
        id: n.id(),
        decimicro_lat: n.decimicro_lat(),
        decimicro_lon: n.decimicro_lon(),
        tags: n
            .tags()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect(),
        metadata: element_metadata(crate::ElementKind::Node, n.id(), &n.info())?
            .map(OwnedMetadata::from_borrowed),
    })
}

pub(crate) fn read_way(w: &crate::Way<'_>) -> Result<OwnedWay> {
    Ok(OwnedWay {
        id: w.id(),
        tags: w
            .tags()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect(),
        refs: w.refs().collect(),
        locations: Vec::new(),
        metadata: element_metadata(crate::ElementKind::Way, w.id(), &w.info())?
            .map(OwnedMetadata::from_borrowed),
    })
}

/// Like [`read_way`] but also captures the way's inline node coordinates
/// (LocationsOnWays). Used by `repack` when the input header declares the
/// `LocationsOnWays` feature so the trailing-slice / merge path can
/// round-trip coordinates. If the source way carries no inline coordinates
/// (empty `node_locations()`), `locations` stays empty and the write path
/// falls back to the plain `add_way` encoding for that way.
pub(crate) fn read_way_with_locations(w: &crate::Way<'_>) -> Result<OwnedWay> {
    let mut way = read_way(w)?;
    way.locations = w
        .node_locations()
        .map(|loc| (loc.decimicro_lat(), loc.decimicro_lon()))
        .collect();
    Ok(way)
}

/// A bad role string index is an error naming the relation and member, as in
/// [`relation_members_into`].
pub(crate) fn read_relation(r: &crate::Relation<'_>) -> Result<OwnedRelation> {
    let members = r
        .members()
        .enumerate()
        .map(|(position, m)| {
            let role = m
                .role()
                .map_err(|e| bad_role(e, r.id(), position, m.role_sid))?;
            Ok(OwnedMember {
                id: m.id,
                role: role.to_owned(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(OwnedRelation {
        id: r.id(),
        tags: r
            .tags()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect(),
        members,
        metadata: element_metadata(crate::ElementKind::Relation, r.id(), &r.info())?
            .map(OwnedMetadata::from_borrowed),
    })
}

// ---------------------------------------------------------------------------
// Write functions - write owned elements into a BlockBuilder whose filled
// blocks go to a local `Vec<OwnedBlock>` (`ensure_*_capacity_local` /
// `flush_local`), so a worker thread never touches the writer.
// ---------------------------------------------------------------------------

pub(crate) fn write_single_node_local(
    node: &OwnedNode,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
) -> crate::Result<()> {
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
) -> crate::Result<()> {
    crate::commands::ensure_way_capacity_local(bb, output)?;
    let meta = owned_to_metadata(way.metadata.as_ref());
    if way.locations.len() == way.refs.len() && !way.locations.is_empty() {
        bb.add_way_with_locations(
            way.id,
            way.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())),
            &way.refs,
            &way.locations,
            meta.as_ref(),
        );
    } else {
        bb.add_way(
            way.id,
            way.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())),
            &way.refs,
            meta.as_ref(),
        );
    }
    Ok(())
}

pub(crate) fn write_single_relation_local(
    rel: &OwnedRelation,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
) -> crate::Result<()> {
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

#[cfg(test)]
mod tests {
    use super::read_dense_node;
    use crate::block_builder::{BlockBuilder, Metadata};
    use crate::{Element, PrimitiveBlock};

    #[test]
    fn corrupt_dense_user_sid_is_refused() {
        let mut builder = BlockBuilder::new();
        builder.add_node(
            1,
            10,
            20,
            [],
            Some(&Metadata {
                version: 7,
                timestamp: 11_000,
                changeset: 13,
                uid: 17,
                user: "user",
                visible: false,
            }),
        );
        let bytes = builder
            .take()
            .expect("encode block")
            .expect("block bytes")
            .to_vec();
        let block = PrimitiveBlock::from_vec(bytes).expect("parse block");
        let mut dense = match block.elements().next().expect("dense node") {
            Element::DenseNode(node) => node,
            _ => panic!("expected dense node"),
        };
        dense.set_test_user_sid(i32::MAX);

        let Err(err) = read_dense_node(&dense) else {
            panic!("a user string index past the string table must be refused, not emptied");
        };
        let report = err.report();
        assert!(
            report.contains("node 1: user name string index 2147483647"),
            "the refusal names the element and the index: {report}"
        );
        assert!(
            matches!(
                err.kind(),
                crate::ErrorKind::StringtableIndexOutOfBounds { .. }
            ),
            "{report}"
        );
    }
}
