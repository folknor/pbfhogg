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
    /// inline coordinates (the common case; the reader goes by the way's
    /// payload, not the header's `LocationsOnWays` declaration). `read_way`
    /// always leaves this empty; `read_way_with_locations` (repack) and
    /// `read_declared_way` (the sort and `cat --dedupe` sweep over declaring
    /// inputs) populate it. The `write_single_way*` helpers switch to
    /// `add_way_with_locations` when this is non-empty; a non-empty vector
    /// whose length differs from `refs` is refused as an internal error.
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
        // Owned elements are never cleaned, so the builder's escape rule can
        // be applied here once: a versioned slot keeps its 0 as a claim.
        if dense.writes_plain() {
            return Self::from_plain(dense.plain_claim_metadata());
        }
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
                explicit: DenseColumns::default(),
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
    pub(crate) fn as_partial(&self) -> PartialMetadata<'_> {
        match self.as_borrowed() {
            Ok(DenseMetadata {
                meta: m,
                columns: c,
                ..
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
    pub(crate) fn id(&self) -> i64 {
        match self {
            Self::Node(n) => n.id,
            Self::Way(w) => w.id,
            Self::Relation(r) => r.id,
        }
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

/// A way's inline locations against its refs, checked before the write that
/// would encode them: one location per ref, or none when `allow_empty`.
///
/// Every location vector here comes from the decoder, which already refused
/// lat/lon arrays whose length disagrees with the refs, so a disagreement is
/// a pbfhogg transformation bug, not input: `Error::internal` (ADR-0021).
/// The writer used to fall back to a plain way on a nonempty mismatch, which
/// would have dropped the coordinates silently; the builder's own length
/// guard poisons instead, but surfaces as `invalid_data`, the wrong class.
pub(crate) fn check_way_locations(
    id: i64,
    refs: usize,
    locations: usize,
    allow_empty: bool,
) -> Result<()> {
    if locations == refs || (allow_empty && locations == 0) {
        return Ok(());
    }
    Err(crate::Error::internal(format!(
        "way {id}: {refs} node ref(s) but {locations} inline location(s) reached the writer"
    )))
}

/// Add an owned way: with its inline locations when it carries any, plain
/// otherwise. The caller has run [`check_way_locations`], so nonempty
/// locations match the refs one for one.
fn add_owned_way(bb: &mut BlockBuilder, way: &OwnedWay) {
    let tags = way.tags.iter().map(|(k, v)| (k.as_str(), v.as_str()));
    let meta = way.metadata.as_ref().map(OwnedMetadata::as_partial);
    if !way.locations.is_empty() {
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
/// reader's Osmosis -1 normalisation; the timestamp as
/// [`output_timestamp_seconds`] converts it), so re-encoding through the `add_*_partial` builder methods
/// neither invents a 0 for an absent field nor drops the fields of a
/// versionless `Info`. `None` when the `Info` carries no field at all.
///
/// A user name that is written is resolved, never substituted: a user string
/// index past the block's string table, or negative, is an error naming the
/// element and the index, not an empty user. The refusal stands even where
/// the caller later clears the user, except for `cat --clean user`, which
/// uses [`element_metadata_for_cat`] and never resolves it.
pub(crate) fn element_metadata<'a>(
    kind: crate::ElementKind,
    id: i64,
    info: &crate::Info<'a>,
) -> Result<Option<PartialMetadata<'a>>> {
    element_metadata_impl::<true, true>(kind, id, info)
}

/// [`element_metadata`] for `cat --clean`, which drops what it cleans
/// before reading it: under `clean_user` a present user is not resolved (it
/// comes back as `Some("")`, an absent one stays `None`), and under
/// `clean_timestamp` a present timestamp is not converted (it comes back as
/// `Some(0)`), so a bad user string index or a timestamp whose scaling
/// overflows does not fail the run. See [`dense_node_metadata_for_cat`] for
/// why this exists.
pub(crate) fn element_metadata_for_cat<'a>(
    kind: crate::ElementKind,
    id: i64,
    info: &crate::Info<'a>,
    clean_user: bool,
    clean_timestamp: bool,
) -> Result<Option<PartialMetadata<'a>>> {
    match (clean_user, clean_timestamp) {
        (false, false) => element_metadata_impl::<true, true>(kind, id, info),
        (true, false) => element_metadata_impl::<false, true>(kind, id, info),
        (false, true) => element_metadata_impl::<true, false>(kind, id, info),
        (true, true) => element_metadata_impl::<false, false>(kind, id, info),
    }
}

/// The `date_granularity` every block pbfhogg writes declares: the
/// protobuf default, 1000 ms.
pub(crate) const OUTPUT_DATE_GRANULARITY: i64 = 1000;

/// A stored timestamp re-expressed in the whole seconds an output block at
/// [`OUTPUT_DATE_GRANULARITY`] stores.
///
/// At the same granularity the raw value is copied verbatim, so the output
/// reads back exactly the claim the input made, an overflowing scaling
/// included (re-encoding does not judge it; each output that reads the
/// claim does). Any other granularity is scaled exactly to milliseconds and
/// rounded down to the whole second holding that instant (as time-filter
/// and the OSC writer round), so a sub-second part is lost: the OSM data
/// model, and every rebuilt block, holds whole seconds. A scaling that
/// overflows is refused rather than written as a different, valid claim,
/// and so is a second the output could not scale back (`i64::MIN` ms
/// rounds down past it).
///
/// Rounding can turn a dense slot's non-zero claim into 0, which a
/// versionless dense slot reads as padding; the dense conversions flag that
/// ([`crate::DenseNodeInfo::output_timestamp`]) and the builder writes such
/// a node plain when it is written without a version.
#[inline]
pub(crate) fn output_timestamp_seconds(raw: i64, date_granularity: i64) -> Result<i64> {
    if date_granularity == OUTPUT_DATE_GRANULARITY {
        Ok(raw)
    } else {
        rescale_timestamp_seconds(raw, date_granularity)
    }
}

#[cold]
#[inline(never)]
fn rescale_timestamp_seconds(raw: i64, date_granularity: i64) -> Result<i64> {
    let seconds =
        crate::read::claims::scale_milli_timestamp(raw, date_granularity)?.div_euclid(1000);
    if seconds.checked_mul(OUTPUT_DATE_GRANULARITY).is_none() {
        return Err(crate::error::new_wire_error(
            "timestamp rounded down to whole seconds no longer fits i64 milliseconds; \
             the blob's timestamp or granularity is corrupt",
        ));
    }
    Ok(seconds)
}

#[inline]
fn element_metadata_impl<'a, const RESOLVE_USER: bool, const CONVERT_TIMESTAMP: bool>(
    kind: crate::ElementKind,
    id: i64,
    info: &crate::Info<'a>,
) -> Result<Option<PartialMetadata<'a>>> {
    let user = match info.user() {
        None => None,
        Some(_) if !RESOLVE_USER => Some(""),
        Some(user) => Some(user.map_err(|e| bad_user(e, kind, id, info.raw_user_sid()))?),
    };
    let timestamp = if CONVERT_TIMESTAMP {
        info.output_timestamp_seconds()
            .transpose()
            .map_err(|e| e.context(format!("{} {id}: timestamp", kind.as_str())))?
    } else {
        info.milli_timestamp().map(|_| 0)
    };
    let meta = PartialMetadata {
        version: info.version(),
        timestamp,
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
/// from a timestamp under a non-default `date_granularity`, which
/// [`output_timestamp_seconds`] rounds down to whole seconds (refusing one
/// whose scaling overflows; a claim that rounds to 0 is flagged in
/// `DenseMetadata::explicit`, and written plain when versionless). The
/// user name is resolved as in [`element_metadata`]: a bad user string index
/// is an error, not an empty user.
pub(crate) fn dense_node_metadata<'a>(
    dn: &'a crate::DenseNode<'a>,
) -> Result<Option<DenseMetadata<'a>>> {
    dense_node_metadata_impl::<true, true>(dn)
}

/// [`dense_node_metadata`] for `cat --clean`: under `clean_user` the user is
/// not resolved (it comes back empty), under `clean_timestamp` the
/// timestamp is not converted (it comes back 0).
///
/// `cat` is the recovery command, and `--clean user` drops the user, so the
/// user string index of an element it is about to discard is never looked up:
/// a file whose user index is out of range can be rescued with it. The same
/// holds for `--clean timestamp` and a timestamp whose scaling overflows.
/// Every other re-encoding command and flag keeps the refusals, because it
/// writes or acts on the field. Roles have no `--clean` and stay refused.
pub(crate) fn dense_node_metadata_for_cat<'a>(
    dn: &'a crate::DenseNode<'a>,
    clean_user: bool,
    clean_timestamp: bool,
) -> Result<Option<DenseMetadata<'a>>> {
    match (clean_user, clean_timestamp) {
        (false, false) => dense_node_metadata_impl::<true, true>(dn),
        (true, false) => dense_node_metadata_impl::<false, true>(dn),
        (false, true) => dense_node_metadata_impl::<true, false>(dn),
        (true, true) => dense_node_metadata_impl::<false, false>(dn),
    }
}

#[inline]
fn dense_node_metadata_impl<'a, const RESOLVE_USER: bool, const CONVERT_TIMESTAMP: bool>(
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
    let (timestamp, zero_timestamp_claim) = if CONVERT_TIMESTAMP {
        dense_output_timestamp(dn.id(), info)?
    } else {
        (0, false)
    };
    Ok(Some(DenseMetadata {
        meta: Metadata {
            version: reencoded_version(info.version()),
            timestamp,
            changeset: map_sentinel(info.changeset()),
            uid: info.uid(),
            user,
            visible: info.visible(),
        },
        columns: info.columns().for_reencode(),
        explicit: if zero_timestamp_claim {
            DenseColumns::TIMESTAMP_ONLY
        } else {
            DenseColumns::default()
        },
    }))
}

/// A dense slot's output timestamp and its rounded-to-zero flag
/// (`DenseNodeInfo::output_timestamp`), the error naming the node.
#[inline]
fn dense_output_timestamp(id: i64, info: &crate::DenseNodeInfo<'_>) -> Result<(i64, bool)> {
    info.output_timestamp()
        .map_err(|e| e.context(format!("node {id}: timestamp")))
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
/// The timestamp is the source's, as [`output_timestamp_seconds`] converts
/// it.
pub(crate) fn element_raw_partial_metadata(
    kind: crate::ElementKind,
    id: i64,
    info: &crate::Info<'_>,
) -> Result<Option<RawPartialMetadata>> {
    let meta = RawPartialMetadata {
        version: info.version(),
        timestamp: info
            .output_timestamp_seconds()
            .transpose()
            .map_err(|e| e.context(format!("{} {id}: timestamp", kind.as_str())))?,
        changeset: info.changeset(),
        uid: info.uid(),
        user_sid: info.raw_user_sid(),
        visible: info.visible_opt(),
    };
    Ok((meta != RawPartialMetadata::default()).then_some(meta))
}

/// Extract [`RawMetadata`] from a [`DenseNode`](crate::DenseNode), preserving
/// the raw string table index for the user name, under
/// [`dense_node_metadata`]'s rules, the plain escape included. For that
/// escape the user index is judged by the name it resolves to, as
/// `DenseNodeInfo::claims` judges it: an index naming the empty string is
/// written as 0, so the plain node does not claim an empty user.
pub(crate) fn dense_node_raw_metadata(dn: &crate::DenseNode<'_>) -> Result<Option<RawMetadata>> {
    let Some(info) = dn.info() else {
        return Ok(None);
    };
    let (timestamp, zero_timestamp_claim) = dense_output_timestamp(dn.id(), info)?;
    let version = reencoded_version(info.version());
    let mut user_sid = info.raw_user_sid();
    // Only the plain escape (a rounded-away claim on a versionless slot)
    // judges the user by its name; the dense route copies the index.
    if zero_timestamp_claim && version < 1 && user_sid != 0 {
        let name = info
            .user()
            .map_err(|e| bad_user(e, crate::ElementKind::Node, dn.id(), Some(user_sid)))?;
        if name.is_empty() {
            user_sid = 0;
        }
    }
    Ok(Some(RawMetadata {
        version,
        timestamp,
        changeset: map_sentinel(info.changeset()),
        uid: info.uid(),
        user_sid,
        visible: info.visible(),
        columns: info.columns().for_reencode(),
        zero_timestamp_claim,
    }))
}

// ---------------------------------------------------------------------------
// Read functions - convert parsed elements to owned
// ---------------------------------------------------------------------------

// The metadata of every owned conversion comes from `element_metadata` /
// `dense_node_metadata`, so an owned re-encode and a borrowed one write the
// same metadata and refuse the same bad user name. Tags go through the
// infallible tag iterators, which cannot shorten a list: block construction
// refused any tag string index past the table.

// Owned nodes hold decimicrodegrees, so a node from a block at a finer
// `granularity` or a non-zero offset is truncated toward zero here. That is
// the rebuild contract, not an oversight (docs/cli/commands.md "PBF rebuild
// precision"): the OSM data model, libosmium and `diff` hold 1e-7 degrees,
// no known producer writes another framing, and carrying the source framing
// into the output cannot represent a way whose locations come from blocks
// of different framings (a refreshed or newly located way), so preserving
// it was considered and declined. Only `digest --form full` sees the
// difference against a copied blob.
pub(crate) fn read_dense_node(dn: &crate::DenseNode<'_>) -> Result<OwnedNode> {
    Ok(OwnedNode {
        id: dn.id(),
        decimicro_lat: dn.decimicro_lat()?,
        decimicro_lon: dn.decimicro_lon()?,
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
        decimicro_lat: n.decimicro_lat()?,
        decimicro_lon: n.decimicro_lon()?,
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
        .map(|loc| Ok((loc.decimicro_lat()?, loc.decimicro_lon()?)))
        .collect::<Result<_>>()?;
    Ok(way)
}

/// A way's inline coordinates, into `out`, for a writer whose output header
/// declares `LocationsOnWays` because its inputs do: one location per ref,
/// or the run is refused. A declaring input whose way carries refs without
/// coordinates would otherwise be re-encoded into a way the output header
/// misdescribes. The decoder already refuses arrays whose lengths disagree
/// with each other or with the refs, so the one case left is a way with
/// refs and no arrays. `refs` is the way's ref count, which the caller has
/// already decoded.
pub(crate) fn declared_way_locations(
    w: &crate::Way<'_>,
    refs: usize,
    out: &mut Vec<(i32, i32)>,
) -> Result<()> {
    out.clear();
    for loc in w.node_locations() {
        out.push((loc.decimicro_lat()?, loc.decimicro_lon()?));
    }
    if out.len() != refs {
        return Err(crate::Error::invalid_data(format!(
            "way {}: the input declares LocationsOnWays, but the way has {refs} node ref(s) \
             and {} inline location(s)",
            w.id(),
            out.len()
        )));
    }
    Ok(())
}

/// [`read_way`] with [`declared_way_locations`]: the owned form of a way
/// whose output keeps the `LocationsOnWays` declaration.
pub(crate) fn read_declared_way(w: &crate::Way<'_>) -> Result<OwnedWay> {
    let mut way = read_way(w)?;
    declared_way_locations(w, way.refs.len(), &mut way.locations)?;
    Ok(way)
}

/// Write an owned way read by [`read_declared_way`]: with its locations
/// always, never falling back to a plain way.
pub(crate) fn write_declared_way_local(
    way: &OwnedWay,
    bb: &mut BlockBuilder,
    output: &mut Vec<OwnedBlock>,
) -> crate::Result<()> {
    check_way_locations(way.id, way.refs.len(), way.locations.len(), false)?;
    crate::commands::ensure_way_capacity_local(bb, output)?;
    let tags = way.tags.iter().map(|(k, v)| (k.as_str(), v.as_str()));
    let meta = way.metadata.as_ref().map(OwnedMetadata::as_partial);
    bb.add_way_with_locations_partial(way.id, tags, &way.refs, &way.locations, meta.as_ref());
    Ok(())
}

/// Whether a command that decodes and re-encodes whole ways writes their
/// inline coordinates. A way written by such a command keeps every ref it
/// had, so each kept coordinate is the one its input recorded beside that
/// ref; a command that alters refs, or whose coordinates would no longer be
/// true of the output (time-filter), must not use [`WayCoords::Keep`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WayCoords {
    /// Ways are written with refs only.
    Drop,
    /// Ways are written with their inline coordinates, one per ref; a way
    /// with refs and no coordinates is refused (see [`declared_way_locations`]).
    Keep,
}

impl WayCoords {
    /// Keep exactly when the one input declares `LocationsOnWays`.
    pub(crate) fn of_input(header: &crate::HeaderBlock) -> Self {
        if header.has_locations_on_ways() {
            Self::Keep
        } else {
            Self::Drop
        }
    }

    /// The header fact of a command that re-encodes every way it writes
    /// under this policy.
    pub(crate) fn rewritten(self) -> crate::commands::output_header::Ways {
        match self {
            Self::Drop => crate::commands::output_header::Ways::Reencoded,
            Self::Keep => crate::commands::output_header::Ways::ReencodedWithLocations,
        }
    }
}

/// The one encoder for a way re-encoded whole under a [`WayCoords`] policy.
/// [`WayScratch::prepare`] decodes the refs and, under `Keep`, the
/// coordinates from the same way, so the pair is positional by
/// construction; [`WayScratch::write`] may then add that way to any number of
/// builders (multi-extract fans one decoded way out to several regions).
#[derive(Debug, Default)]
pub(crate) struct WayScratch {
    id: i64,
    keep: bool,
    refs: Vec<i64>,
    locations: Vec<(i32, i32)>,
}

impl WayScratch {
    pub(crate) fn prepare(&mut self, w: &crate::Way<'_>, coords: WayCoords) -> Result<()> {
        self.id = w.id();
        self.refs.clear();
        self.refs.extend(w.refs());
        self.keep = coords == WayCoords::Keep;
        if self.keep {
            declared_way_locations(w, self.refs.len(), &mut self.locations)?;
        } else {
            self.locations.clear();
        }
        Ok(())
    }

    /// Add the prepared way to `bb` with `tags` and `meta`; the caller has
    /// ensured way capacity.
    pub(crate) fn write<'t>(
        &self,
        bb: &mut BlockBuilder,
        tags: impl IntoIterator<Item = (&'t str, &'t str)>,
        meta: Option<&PartialMetadata<'_>>,
    ) {
        if self.keep {
            bb.add_way_with_locations_partial(self.id, tags, &self.refs, &self.locations, meta);
        } else {
            bb.add_way_partial(self.id, tags, &self.refs, meta);
        }
    }
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
    check_way_locations(way.id, way.refs.len(), way.locations.len(), true)?;
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

    /// A nonempty location vector that disagrees with the refs is an
    /// internal error at the write boundary, never a silent plain way; empty
    /// passes only where the writer allows a way without coordinates.
    #[test]
    fn mismatched_way_locations_are_an_internal_error() {
        assert!(super::check_way_locations(1, 3, 3, false).is_ok());
        assert!(super::check_way_locations(1, 0, 0, false).is_ok());
        assert!(super::check_way_locations(1, 3, 0, true).is_ok());
        for (refs, locations, allow_empty) in [(3, 0, false), (3, 2, true)] {
            let err = super::check_way_locations(7, refs, locations, allow_empty)
                .expect_err("mismatch must be refused");
            assert!(
                matches!(err.kind(), crate::error::ErrorKind::Internal { .. }),
                "{err}"
            );
            assert!(err.to_string().contains("way 7"), "{err}");
        }
    }

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
                            n.decimicro_lat().unwrap(),
                            n.decimicro_lon().unwrap(),
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

    /// `bytes` reparsed with a `date_granularity` (`PrimitiveBlock` field
    /// 18) appended; a later scalar field replaces an earlier one, and the
    /// builder writes none.
    fn with_date_granularity(mut bytes: Vec<u8>, granularity: u8) -> PrimitiveBlock {
        assert!(granularity < 0x80, "one-byte varint");
        bytes.extend_from_slice(&[0x90, 0x01, granularity]);
        PrimitiveBlock::from_vec(bytes).unwrap()
    }

    /// The default granularity copies verbatim (an overflowing claim stays
    /// the same refused claim); any other rounds the exact milliseconds down
    /// to whole seconds and refuses what the output could not hold.
    #[test]
    fn output_timestamps_round_down_and_refuse_overflow() {
        use super::output_timestamp_seconds as out;
        assert_eq!(out(1_700_000_000, 1000).unwrap(), 1_700_000_000);
        assert_eq!(out(i64::MAX, 1000).unwrap(), i64::MAX);
        assert_eq!(out(1_500, 1).unwrap(), 1);
        assert_eq!(out(999, 1).unwrap(), 0);
        assert_eq!(out(-1, 1).unwrap(), -1, "floor, not truncation toward zero");
        assert_eq!(out(-1_000, 1).unwrap(), -1);
        assert_eq!(out(-1_001, 1).unwrap(), -2);
        assert_eq!(out(7, 0).unwrap(), 0);
        crate::test_support::assert_err_kind!(
            out(i64::MAX, 2),
            crate::ErrorKind::WireFormat { msg } if msg.contains("overflows i64 milliseconds"),
            "a scaling that overflows is refused"
        );
        crate::test_support::assert_err_kind!(
            out(i64::MIN, 1),
            crate::ErrorKind::WireFormat { msg } if msg.contains("no longer fits i64 milliseconds"),
            "i64::MIN ms rounds down to a second that does not scale back"
        );
        assert_eq!(out(i64::MIN + 808, 1).unwrap(), i64::MIN / 1000);
    }

    /// A versionless dense slot claiming 500 ms under granularity 1 rounds
    /// to 0 s, which a dense slot would read as padding: every re-encode
    /// route (borrowed, owned, raw) writes it plain with an explicit 0, and a
    /// versioned slot, whose 0 is a claim, stays dense.
    #[test]
    fn a_versionless_sub_second_claim_rounding_to_zero_is_written_plain() {
        let mut src = BlockBuilder::new();
        for (id, version) in [(1, 0), (2, 3)] {
            src.add_node(
                id,
                0,
                0,
                [],
                Some(&Metadata {
                    version,
                    timestamp: 500,
                    changeset: 0,
                    uid: 0,
                    user: "",
                    visible: true,
                }),
            );
        }
        let block = with_date_granularity(src.take().unwrap().unwrap().to_vec(), 1);

        let mut borrowed = BlockBuilder::new();
        let mut owned = BlockBuilder::new();
        let mut raw = BlockBuilder::new();
        raw.pre_seed_string_table(&block).unwrap();
        for element in block.elements() {
            let Element::DenseNode(dn) = element else {
                panic!("dense source");
            };
            let claimed = dn.info().unwrap().claimed_milli_timestamp();
            assert_eq!(claimed.unwrap().unwrap(), 500, "the source claims 500 ms");
            let meta = super::dense_node_metadata(&dn).unwrap();
            borrowed.add_node_dense(dn.id(), 0, 0, [], meta.as_ref());
            super::add_owned_node(&mut owned, &read_dense_node(&dn).unwrap());
            let raw_meta = super::dense_node_raw_metadata(&dn).unwrap();
            raw.add_node_raw(dn.id(), 0, 0, std::iter::empty(), raw_meta.as_ref())
                .unwrap();
        }
        for (route, bb) in [("borrowed", borrowed), ("owned", owned), ("raw", raw)] {
            let mut bb = bb;
            let out = one_block(&mut bb);
            let mut elements = out.elements();
            let Some(Element::Node(plain)) = elements.next() else {
                panic!("{route}: the versionless node is written plain");
            };
            // The source's zero changeset and uid, empty user (at a non-zero
            // string index) and visible slot are padding, so the plain node
            // claims the timestamp alone.
            assert_eq!(
                (plain.id(), info_fields(&plain.info())),
                (1, (None, Some(0), None, None, None, None)),
                "{route}: the versionless claim survives as an explicit 0, alone"
            );
            let Some(Element::DenseNode(dense)) = elements.next() else {
                panic!("{route}: the versioned node stays dense");
            };
            let claimed = dense.info().unwrap().claimed_milli_timestamp();
            assert_eq!((dense.id(), claimed.map(|t| t.unwrap())), (2, Some(0)));
        }
    }

    /// `--clean version` on a versioned slot whose claim rounds to 0 s leaves
    /// it versionless, so it takes the plain escape too; `--clean timestamp`
    /// writes the cleaned 0 as padding, as it always has.
    #[test]
    fn cleaning_decides_the_escape_from_the_cleaned_slot() {
        let mut src = BlockBuilder::new();
        src.add_node(
            1,
            0,
            0,
            [],
            Some(&Metadata {
                version: 3,
                timestamp: 500,
                changeset: 9,
                uid: 4,
                user: "u",
                visible: true,
            }),
        );
        let block = with_date_granularity(src.take().unwrap().unwrap().to_vec(), 1);
        let Some(Element::DenseNode(dn)) = block.elements().next() else {
            panic!("dense node");
        };
        let clean = |version, timestamp| crate::commands::cat::CleanAttrs {
            version,
            changeset: false,
            timestamp,
            uid: false,
            user: false,
        };
        let write = |c: &crate::commands::cat::CleanAttrs| {
            let meta = crate::commands::clean_metadata(super::dense_node_metadata(&dn).unwrap(), c);
            let mut bb = BlockBuilder::new();
            bb.add_node_dense(1, 0, 0, [], meta.as_ref());
            one_block(&mut bb)
        };
        let out = write(&clean(true, false));
        let Some(Element::Node(n)) = out.elements().next() else {
            panic!("the cleaned versionless node is written plain");
        };
        assert_eq!(
            info_fields(&n.info()),
            (None, Some(0), Some(9), Some(4), Some("u".to_owned()), None)
        );
        let out = write(&clean(false, true));
        let Some(Element::DenseNode(d)) = out.elements().next() else {
            panic!("a versioned node with a cleaned timestamp stays dense");
        };
        assert_eq!(
            d.info()
                .unwrap()
                .claimed_milli_timestamp()
                .unwrap()
                .unwrap(),
            0
        );
    }

    /// A versioned slot claims its present zeros (the epoch, changeset 0,
    /// uid 0, an empty user); `--clean version` leaves it versionless, where
    /// a dense slot would read them as padding, so a node holding one is
    /// written plain with every kept claim explicit. A cleaned field is the
    /// padding 0 osmium writes and is not claimed, so cleaning a timestamp
    /// never fabricates an epoch; a slot with no kept zero stays dense.
    #[test]
    fn clean_version_keeps_the_zero_claims_of_a_versioned_slot() {
        let mut src = BlockBuilder::new();
        for (id, timestamp, changeset, uid, user) in
            [(1, 0, 0, 0, ""), (2, 1_700_000_000, 9, 4, "u")]
        {
            src.add_node(
                id,
                0,
                0,
                [],
                Some(&Metadata {
                    version: 3,
                    timestamp,
                    changeset,
                    uid,
                    user,
                    visible: true,
                }),
            );
        }
        let block = one_block(&mut src);
        let mut elements = block.elements();
        let (Some(Element::DenseNode(zero)), Some(Element::DenseNode(set))) =
            (elements.next(), elements.next())
        else {
            panic!("dense nodes");
        };
        let clean = |version, timestamp, changeset, uid, user| crate::commands::cat::CleanAttrs {
            version,
            changeset,
            timestamp,
            uid,
            user,
        };
        let write = |dn: &crate::DenseNode<'_>, c: &crate::commands::cat::CleanAttrs| {
            let meta = crate::commands::clean_metadata(super::dense_node_metadata(dn).unwrap(), c);
            let mut bb = BlockBuilder::new();
            bb.add_node_dense(dn.id(), 0, 0, [], meta.as_ref());
            one_block(&mut bb)
        };
        let plain = |out: &PrimitiveBlock| {
            let Some(Element::Node(n)) = out.elements().next() else {
                panic!("written plain");
            };
            info_fields(&n.info())
        };
        assert_eq!(
            plain(&write(&zero, &clean(true, false, false, false, false))),
            (None, Some(0), Some(0), Some(0), Some(String::new()), None),
            "every kept zero stays claimed"
        );
        assert_eq!(
            plain(&write(&zero, &clean(true, false, false, true, false))),
            (None, Some(0), Some(0), None, Some(String::new()), None),
            "the cleaned uid is padding, the kept fields are claims"
        );
        assert_eq!(
            plain(&write(&zero, &clean(true, true, false, false, false))),
            (None, None, Some(0), Some(0), Some(String::new()), None),
            "a cleaned timestamp is not turned into an epoch claim"
        );
        let out = write(&zero, &clean(true, true, true, true, true));
        let Some(Element::DenseNode(d)) = out.elements().next() else {
            panic!("an all-cleaned node keeps nothing to claim and stays dense");
        };
        assert!(d.info().unwrap().claimed_milli_timestamp().is_none());
        let out = write(&set, &clean(true, false, false, false, false));
        let Some(Element::DenseNode(d)) = out.elements().next() else {
            panic!("a node with no kept zero stays dense");
        };
        assert_eq!(d.info().unwrap().claimed_version(), None);
        assert_eq!(
            d.info()
                .unwrap()
                .claimed_milli_timestamp()
                .map(|t| t.unwrap()),
            Some(1_700_000_000_000)
        );
    }

    /// A timestamp whose millisecond scaling overflows under a non-default
    /// granularity is refused by the re-encode, naming the node.
    #[test]
    fn an_overflowing_rescaled_timestamp_is_refused() {
        let mut src = BlockBuilder::new();
        src.add_node(
            7,
            0,
            0,
            [],
            Some(&Metadata {
                version: 1,
                timestamp: i64::MAX / 2,
                changeset: 1,
                uid: 1,
                user: "u",
                visible: true,
            }),
        );
        let block = with_date_granularity(src.take().unwrap().unwrap().to_vec(), 4);
        let Some(Element::DenseNode(dn)) = block.elements().next() else {
            panic!("dense node");
        };
        let Err(err) = super::dense_node_metadata(&dn) else {
            panic!("an overflowing scaling must be refused, not wrapped");
        };
        assert!(
            err.report().contains("node 7: timestamp"),
            "{}",
            err.report()
        );
        crate::test_support::assert_err_kind!(
            super::dense_node_raw_metadata(&dn),
            crate::ErrorKind::WireFormat { .. }
        );
        let cleaned = super::dense_node_metadata_for_cat(&dn, false, true)
            .expect("cat --clean timestamp does not judge the timestamp it drops")
            .unwrap();
        assert_eq!(
            (cleaned.meta.timestamp, cleaned.explicit.is_empty()),
            (0, true)
        );
    }
}
