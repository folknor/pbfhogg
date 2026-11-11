//! PBF-oriented owned element types for decode → re-encode round-trips.
//!
//! Used by the blob-level merge (sort, cat --dedupe) and time_filter for
//! overlap-run and sweep-merge operations.

use crate::DenseColumns;
use crate::block_builder::{
    BlockBuilder, DenseMetadata, MemberData, Metadata, OwnedBlock, PartialMetadata, RawMetadata,
    RawPartialMetadata,
};

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

/// Owned element metadata, keeping where it came from.
///
/// A dense slot and a plain `Info` with the same stored values can make
/// different claims: a version-0 dense slot does not claim its zero uid,
/// changeset or empty user (`DenseNodeInfo::claims`), a plain `Info` that
/// carries them does. So a dense node's metadata stays [`Dense`] and is
/// written dense as decoded, and a plain element's stays [`Plain`], each
/// field present exactly when the source carried it.
///
/// [`Dense`]: OwnedMetadata::Dense
/// [`Plain`]: OwnedMetadata::Plain
pub(crate) enum OwnedMetadata {
    Dense {
        version: i32,
        timestamp: i64,
        changeset: i64,
        uid: i32,
        user: String,
        visible: bool,
        /// The columns the node is written with (`DenseColumns::for_reencode`
        /// of its source group); a field outside them is 0 and unwritten.
        columns: DenseColumns,
    },
    Plain {
        version: Option<i32>,
        timestamp: Option<i64>,
        changeset: Option<i64>,
        uid: Option<i32>,
        user: Option<String>,
        visible: Option<bool>,
    },
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
    meta.as_ref().map_or(0, OwnedMetadata::version_or_zero)
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
    /// Own a dense node's [`Metadata`] from [`dense_node_metadata`], so an
    /// owned element and a borrowed re-encode of the same input write the
    /// same metadata.
    fn from_dense(dense: DenseMetadata<'_>) -> Self {
        let meta = dense.meta;
        Self::Dense {
            version: meta.version,
            timestamp: meta.timestamp,
            changeset: meta.changeset,
            uid: meta.uid,
            user: meta.user.to_owned(),
            visible: meta.visible,
            columns: dense.columns,
        }
    }

    /// Own a plain element's [`PartialMetadata`] from [`element_metadata`].
    fn from_plain(meta: PartialMetadata<'_>) -> Self {
        Self::Plain {
            version: meta.version,
            timestamp: meta.timestamp,
            changeset: meta.changeset,
            uid: meta.uid,
            user: meta.user.map(str::to_owned),
            visible: meta.visible,
        }
    }

    /// The version, 0 when absent: the ordering and dedupe key the sort
    /// sweep shares with its borrowed side.
    pub(crate) fn version_or_zero(&self) -> i32 {
        match self {
            Self::Dense { version, .. } => *version,
            Self::Plain { version, .. } => version.unwrap_or(0),
        }
    }

    /// Visibility, absent read as visible (the PBF default).
    pub(crate) fn visible(&self) -> bool {
        match self {
            Self::Dense { visible, .. } => *visible,
            Self::Plain { visible, .. } => visible.unwrap_or(true),
        }
    }

    /// The metadata as the builder takes it: `Ok` dense metadata with its
    /// columns for a dense node, `Err` partial metadata for a plain element.
    pub(crate) fn as_borrowed(
        &self,
    ) -> std::result::Result<DenseMetadata<'_>, PartialMetadata<'_>> {
        match self {
            Self::Dense {
                version,
                timestamp,
                changeset,
                uid,
                user,
                visible,
                columns,
            } => Ok(DenseMetadata {
                meta: Metadata {
                    version: *version,
                    timestamp: *timestamp,
                    changeset: *changeset,
                    uid: *uid,
                    user,
                    visible: *visible,
                },
                columns: *columns,
            }),
            Self::Plain {
                version,
                timestamp,
                changeset,
                uid,
                user,
                visible,
            } => Err(PartialMetadata {
                version: *version,
                timestamp: *timestamp,
                changeset: *changeset,
                uid: *uid,
                user: user.as_deref(),
                visible: *visible,
            }),
        }
    }

    /// The metadata as partial metadata, for a way or relation (which never
    /// carries dense metadata; a dense value converts field for field, each
    /// present when its column is).
    fn as_partial(&self) -> PartialMetadata<'_> {
        match self.as_borrowed() {
            Ok(DenseMetadata {
                meta: m,
                columns: c,
            }) => PartialMetadata {
                version: c.version().then_some(m.version),
                timestamp: c.timestamp().then_some(m.timestamp),
                changeset: c.changeset().then_some(m.changeset),
                uid: c.uid().then_some(m.uid),
                user: c.user().then_some(m.user),
                visible: c.visible().then_some(m.visible),
            },
            Err(p) => p,
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
        self.metadata().is_none_or(OwnedMetadata::visible)
    }
}

// ---------------------------------------------------------------------------
// Shared helpers: add an owned element to a builder, dense metadata through
// the dense add, plain metadata through the presence-keeping partial adds.
// ---------------------------------------------------------------------------

pub(crate) fn add_owned_node(bb: &mut BlockBuilder, node: &OwnedNode) {
    let tags = node.tags.iter().map(|(k, v)| (k.as_str(), v.as_str()));
    match node.metadata.as_ref().map(OwnedMetadata::as_borrowed) {
        None => bb.add_node(node.id, node.decimicro_lat, node.decimicro_lon, tags, None),
        Some(Ok(full)) => bb.add_node_dense(
            node.id,
            node.decimicro_lat,
            node.decimicro_lon,
            tags,
            Some(&full),
        ),
        Some(Err(partial)) => bb.add_node_partial(
            node.id,
            node.decimicro_lat,
            node.decimicro_lon,
            tags,
            Some(&partial),
        ),
    }
}

/// Add an owned way: with its inline locations when it carries one per ref,
/// plain otherwise.
pub(crate) fn add_owned_way(bb: &mut BlockBuilder, way: &OwnedWay) {
    let tags = way.tags.iter().map(|(k, v)| (k.as_str(), v.as_str()));
    let meta = way.metadata.as_ref().map(OwnedMetadata::as_partial);
    if way.locations.len() == way.refs.len() && !way.locations.is_empty() {
        bb.add_way_with_locations_partial(way.id, tags, &way.refs, &way.locations, meta.as_ref());
    } else {
        bb.add_way_partial(way.id, tags, &way.refs, meta.as_ref());
    }
}

pub(crate) fn add_owned_relation(bb: &mut BlockBuilder, rel: &OwnedRelation) {
    let members: Vec<MemberData<'_>> = rel
        .members
        .iter()
        .map(|m| MemberData {
            id: m.id,
            role: &m.role,
        })
        .collect();
    let meta = rel.metadata.as_ref().map(OwnedMetadata::as_partial);
    bb.add_relation_partial(
        rel.id,
        rel.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())),
        &members,
        meta.as_ref(),
    );
}

// ---------------------------------------------------------------------------
// Borrowed-element to Metadata extraction
// ---------------------------------------------------------------------------

/// The version a dense slot is re-encoded with: the Osmosis -1 "absent"
/// sentinel becomes 0, as libosmium reads it. Both are "no version" to
/// `DenseNodeInfo::claims`, so the slot claims the same; any other value,
/// a version below -1 included, is written as decoded.
#[inline]
fn reencoded_version(version: i32) -> i32 {
    if version == -1 { 0 } else { version }
}

/// The changeset a dense slot is re-encoded with: the Osmosis -1 becomes 0,
/// as libosmium reads it and as `DenseNodeInfo::claims` claims it.
#[inline]
fn map_sentinel(value: i64) -> i64 {
    if value == -1 { 0 } else { value }
}

/// Extract [`PartialMetadata`] from the [`Info`](crate::Info) of the Node,
/// Way or Relation `kind` `id`, for re-encoding.
///
/// Each field is present exactly when the `Info` carries it (after the
/// reader's Osmosis -1 normalisation; the timestamp truncated to whole
/// seconds), so re-encoding through the `add_*_partial` builder methods
/// neither invents a 0 for an absent field nor drops the fields of a
/// versionless `Info`. `None` when the `Info` carries no field at all.
///
/// A user name that is written is resolved, never substituted: a user string
/// index past the block's string table, or negative, is an error naming the
/// element and the index, not an empty user. The refusal stands even where
/// the caller later clears the user, except for `cat --clean user`, which
/// uses [`element_metadata_dropping_user`] and never resolves it.
pub(crate) fn element_metadata<'a>(
    kind: crate::ElementKind,
    id: i64,
    info: &crate::Info<'a>,
) -> Result<Option<PartialMetadata<'a>>> {
    element_metadata_impl::<true>(kind, id, info)
}

/// [`element_metadata`] for `cat --clean user` alone: a present user is not
/// resolved (it comes back as `Some("")`, an absent one stays `None`), so a
/// bad user string index does not fail the run. See
/// [`dense_node_metadata_dropping_user`] for why this exists.
pub(crate) fn element_metadata_dropping_user<'a>(
    kind: crate::ElementKind,
    id: i64,
    info: &crate::Info<'a>,
) -> Result<Option<PartialMetadata<'a>>> {
    element_metadata_impl::<false>(kind, id, info)
}

#[inline]
fn element_metadata_impl<'a, const RESOLVE_USER: bool>(
    kind: crate::ElementKind,
    id: i64,
    info: &crate::Info<'a>,
) -> Result<Option<PartialMetadata<'a>>> {
    let user = match info.user() {
        None => None,
        Some(_) if !RESOLVE_USER => Some(""),
        Some(user) => Some(user.map_err(|e| bad_user(e, kind, id, info.raw_user_sid()))?),
    };
    let meta = PartialMetadata {
        version: info.version(),
        timestamp: info.milli_timestamp().map(|ms| ms / 1000),
        changeset: info.changeset(),
        uid: info.uid(),
        user,
        visible: info.visible_opt(),
    };
    Ok((meta != PartialMetadata::default()).then_some(meta))
}

/// Extract [`DenseMetadata`] from a [`DenseNode`](crate::DenseNode), for
/// re-encoding it dense through `BlockBuilder::add_node_dense`.
///
/// Returns `None` only if the node's group has no `DenseInfo` column. The
/// slot is kept as decoded, columns included
/// ([`DenseColumns::for_reencode`]: a partial-column group keeps its
/// columns, a complete one gains `visible`, read as `true`), except that a
/// version of -1 is written as 0 (both claim no version) and a changeset of
/// -1 as 0 (which `claims` reads it as). So
/// `DenseNodeInfo::claims` reads the output as it read the input, apart
/// from sub-second timestamps, which are truncated to whole seconds. The
/// user name is resolved as in [`element_metadata`]: a bad user string index
/// is an error, not an empty user.
pub(crate) fn dense_node_metadata<'a>(
    dn: &'a crate::DenseNode<'a>,
) -> Result<Option<DenseMetadata<'a>>> {
    dense_node_metadata_impl::<true>(dn)
}

/// [`dense_node_metadata`] for `cat --clean user` alone: the user is not
/// resolved (it comes back empty).
///
/// `cat` is the recovery command, and `--clean user` drops the user, so the
/// user string index of an element it is about to discard is never looked up:
/// a file whose user index is out of range can be rescued with it. Every other
/// re-encoding command and flag keeps the refusal, because it writes or acts
/// on the user. Roles have no `--clean` and stay refused.
pub(crate) fn dense_node_metadata_dropping_user<'a>(
    dn: &'a crate::DenseNode<'a>,
) -> Result<Option<DenseMetadata<'a>>> {
    dense_node_metadata_impl::<false>(dn)
}

#[inline]
fn dense_node_metadata_impl<'a, const RESOLVE_USER: bool>(
    dn: &'a crate::DenseNode<'a>,
) -> Result<Option<DenseMetadata<'a>>> {
    let Some(info) = dn.info() else {
        return Ok(None);
    };
    let user = if RESOLVE_USER {
        info.user().map_err(|e| {
            bad_user(
                e,
                crate::ElementKind::Node,
                dn.id(),
                Some(info.raw_user_sid()),
            )
        })?
    } else {
        ""
    };
    Ok(Some(DenseMetadata {
        meta: Metadata {
            version: reencoded_version(info.version()),
            timestamp: info.milli_timestamp() / 1000,
            changeset: map_sentinel(info.changeset()),
            uid: info.uid(),
            user,
            visible: info.visible(),
        },
        columns: info.columns().for_reencode(),
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

/// Extract [`RawPartialMetadata`] from an [`Info`](crate::Info), each field
/// present exactly when the source carries it, preserving the raw string
/// table index for the user name. `None` when the `Info` carries no field.
/// The timestamp is the source's, scaled by its `date_granularity` and
/// truncated to whole seconds.
pub(crate) fn element_raw_partial_metadata(info: &crate::Info<'_>) -> Option<RawPartialMetadata> {
    let meta = RawPartialMetadata {
        version: info.version(),
        timestamp: info.milli_timestamp().map(|ms| ms / 1000),
        changeset: info.changeset(),
        uid: info.uid(),
        user_sid: info.raw_user_sid(),
        visible: info.visible_opt(),
    };
    (meta != RawPartialMetadata::default()).then_some(meta)
}

/// Extract [`RawMetadata`] from a [`DenseNode`](crate::DenseNode), preserving
/// the raw string table index for the user name, under
/// [`dense_node_metadata`]'s rules.
pub(crate) fn dense_node_raw_metadata(dn: &crate::DenseNode<'_>) -> Option<RawMetadata> {
    dn.info().map(|info| RawMetadata {
        version: reencoded_version(info.version()),
        timestamp: info.milli_timestamp() / 1000,
        changeset: map_sentinel(info.changeset()),
        uid: info.uid(),
        user_sid: info.raw_user_sid(),
        visible: info.visible(),
        columns: info.columns().for_reencode(),
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
        metadata: dense_node_metadata(dn)?.map(OwnedMetadata::from_dense),
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
            .map(OwnedMetadata::from_plain),
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
            .map(OwnedMetadata::from_plain),
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
/// [`Relation::members_into`](crate::Relation::members_into).
pub(crate) fn read_relation(r: &crate::Relation<'_>) -> Result<OwnedRelation> {
    let members = r
        .members()
        .enumerate()
        .map(|(position, m)| {
            let role = r.member_role(position, &m)?;
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
            .map(OwnedMetadata::from_plain),
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
    add_owned_node(bb, node);
    Ok(())
}

pub(crate) fn write_single_way_local(
    way: &OwnedWay,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
) -> crate::Result<()> {
    crate::commands::ensure_way_capacity_local(bb, output)?;
    add_owned_way(bb, way);
    Ok(())
}

pub(crate) fn write_single_relation_local(
    rel: &OwnedRelation,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
) -> crate::Result<()> {
    crate::commands::ensure_relation_capacity_local(bb, output)?;
    add_owned_relation(bb, rel);
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::read_dense_node;
    use crate::block_builder::{BlockBuilder, Metadata, PartialMetadata};
    use crate::{Element, PrimitiveBlock};

    type Fields = (
        Option<i32>,
        Option<i64>,
        Option<i64>,
        Option<i32>,
        Option<String>,
        Option<bool>,
    );

    fn info_fields(info: &crate::Info<'_>) -> Fields {
        (
            info.version(),
            info.milli_timestamp(),
            info.changeset(),
            info.uid(),
            info.user().map(|u| u.unwrap().to_owned()),
            info.visible_opt(),
        )
    }

    fn one_block(bb: &mut BlockBuilder) -> PrimitiveBlock {
        PrimitiveBlock::from_vec(bb.take().unwrap().unwrap().to_vec()).unwrap()
    }

    /// Each plain element's metadata survives decode, own and re-encode with
    /// its field presence: no field is invented as 0 or empty, and a
    /// versionless `Info` keeps its other fields. Both the borrowed
    /// (`element_metadata` + partial add) and the owned (read + write)
    /// routes are checked.
    #[test]
    fn plain_metadata_presence_survives_reencode() {
        let partials = [
            PartialMetadata {
                version: Some(3),
                ..PartialMetadata::default()
            },
            PartialMetadata {
                timestamp: Some(1_600_000_000),
                user: Some("alice"),
                visible: Some(false),
                ..PartialMetadata::default()
            },
            // Complete with version 0: a dense slot would not claim the zeros.
            PartialMetadata {
                version: Some(0),
                timestamp: Some(0),
                changeset: Some(0),
                uid: Some(0),
                user: Some(""),
                visible: None,
            },
        ];
        let mut src = BlockBuilder::new();
        for (id, meta) in (1..).zip(&partials) {
            src.add_way_partial(id, [], &[1, 2], Some(meta));
        }
        let ways = one_block(&mut src);
        let mut src = BlockBuilder::new();
        for (id, meta) in (1..).zip(&partials) {
            src.add_node_partial(id, 10, 20, [], Some(meta));
        }
        let nodes = one_block(&mut src);

        for block in [&ways, &nodes] {
            let expected: Vec<Fields> = block
                .elements()
                .map(|e| match e {
                    Element::Way(w) => info_fields(&w.info()),
                    Element::Node(n) => info_fields(&n.info()),
                    _ => panic!("fixture holds plain nodes and ways only"),
                })
                .collect();
            assert_eq!(expected.len(), 3);

            let mut borrowed = BlockBuilder::new();
            let mut owned = BlockBuilder::new();
            for e in block.elements() {
                match e {
                    Element::Way(w) => {
                        let meta =
                            super::element_metadata(crate::ElementKind::Way, w.id(), &w.info())
                                .unwrap();
                        let refs: Vec<i64> = w.refs().collect();
                        borrowed.add_way_partial(w.id(), w.tags(), &refs, meta.as_ref());
                        super::add_owned_way(&mut owned, &super::read_way(&w).unwrap());
                    }
                    Element::Node(n) => {
                        let meta =
                            super::element_metadata(crate::ElementKind::Node, n.id(), &n.info())
                                .unwrap();
                        borrowed.add_node_partial(
                            n.id(),
                            n.decimicro_lat(),
                            n.decimicro_lon(),
                            n.tags(),
                            meta.as_ref(),
                        );
                        super::add_owned_node(&mut owned, &super::read_node(&n).unwrap());
                    }
                    _ => unreachable!(),
                }
            }
            for out in [one_block(&mut borrowed), one_block(&mut owned)] {
                let got: Vec<Fields> = out
                    .elements()
                    .map(|e| match e {
                        Element::Way(w) => info_fields(&w.info()),
                        Element::Node(n) => info_fields(&n.info()),
                        _ => panic!("partial metadata must stay plain"),
                    })
                    .collect();
                assert_eq!(got, expected);
            }
        }
    }

    /// A dense version-0 slot is owned as dense metadata and written back
    /// dense, not as a plain node claiming its zero fields.
    #[test]
    fn owned_dense_metadata_stays_dense() {
        let mut src = BlockBuilder::new();
        let zero = Metadata {
            version: 0,
            timestamp: 0,
            changeset: 0,
            uid: 0,
            user: "",
            visible: true,
        };
        src.add_node(1, 10, 20, [], Some(&zero));
        let block = one_block(&mut src);
        let Some(Element::DenseNode(dn)) = block.elements().next() else {
            panic!("fixture holds a dense node");
        };
        let mut out = BlockBuilder::new();
        super::add_owned_node(&mut out, &read_dense_node(&dn).unwrap());
        let out = one_block(&mut out);
        let Some(Element::DenseNode(back)) = out.elements().next() else {
            panic!("owned dense metadata must be written dense");
        };
        assert_eq!(back.info().map(crate::DenseNodeInfo::version), Some(0));
    }

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
