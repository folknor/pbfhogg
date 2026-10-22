//! Supported `inspect --get` paths and the work each path requires.

/// The scan requirement for an `inspect --get` path.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InspectGetNeed {
    /// The file/header fast path provides the value.
    Header,
    /// The value requires the ordinary inspect scan.
    Scan,
    /// The value requires the extended inspect scan.
    Extended,
}

/// The value represented by a recognized `inspect --get` path.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InspectGetKeyKind {
    /// The input file name.
    FileName,
    /// The input file size.
    FileSize,
    /// The input file format.
    FileFormat,
    /// The PBF header bounding box.
    HeaderBbox,
    /// The PBF writing program.
    HeaderWritingProgram,
    /// The PBF replication URL.
    HeaderReplicationUrl,
    /// The PBF replication sequence number.
    HeaderReplicationSequence,
    /// The PBF replication timestamp.
    HeaderReplicationTimestamp,
    /// Whether the input has blob-level indexdata.
    Indexed,
    /// The number of input blocks.
    BlocksTotal,
    /// The number of nodes.
    ElementsNodes,
    /// The number of ways.
    ElementsWays,
    /// The number of relations.
    ElementsRelations,
    /// The total number of elements.
    ElementsTotal,
    /// Whether the elements follow standard ordering.
    DataObjectsOrdered,
    /// The first element timestamp.
    DataTimestampFirst,
    /// The last element timestamp.
    DataTimestampLast,
    /// The data bounding box.
    DataBbox,
    /// The number of nodes from the extended scan.
    DataCountNodes,
    /// The number of ways from the extended scan.
    DataCountWays,
    /// The number of relations from the extended scan.
    DataCountRelations,
    /// Whether every object has a version.
    MetadataAllObjectsVersion,
    /// Whether every object has a timestamp.
    MetadataAllObjectsTimestamp,
    /// Whether every object has a changeset.
    MetadataAllObjectsChangeset,
    /// Whether every object has a user ID.
    MetadataAllObjectsUid,
    /// Whether every object has a user name.
    MetadataAllObjectsUser,
    /// Whether any object has a version.
    MetadataSomeObjectsVersion,
    /// Whether any object has a timestamp.
    MetadataSomeObjectsTimestamp,
    /// Whether any object has a changeset.
    MetadataSomeObjectsChangeset,
    /// Whether any object has a user ID.
    MetadataSomeObjectsUid,
    /// Whether any object has a user name.
    MetadataSomeObjectsUser,
}

/// A recognized get path, including its value and scan requirement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct InspectGetKey {
    kind: InspectGetKeyKind,
    need: InspectGetNeed,
}

const INSPECT_GET_KEYS: &[(&str, InspectGetKeyKind, InspectGetNeed)] = &[
    (
        "file.name",
        InspectGetKeyKind::FileName,
        InspectGetNeed::Header,
    ),
    (
        "file.size",
        InspectGetKeyKind::FileSize,
        InspectGetNeed::Header,
    ),
    (
        "file.format",
        InspectGetKeyKind::FileFormat,
        InspectGetNeed::Header,
    ),
    (
        "header.bbox",
        InspectGetKeyKind::HeaderBbox,
        InspectGetNeed::Header,
    ),
    (
        "header.writing_program",
        InspectGetKeyKind::HeaderWritingProgram,
        InspectGetNeed::Header,
    ),
    (
        "header.replication.url",
        InspectGetKeyKind::HeaderReplicationUrl,
        InspectGetNeed::Header,
    ),
    (
        "header.replication.sequence",
        InspectGetKeyKind::HeaderReplicationSequence,
        InspectGetNeed::Header,
    ),
    (
        "header.replication.timestamp",
        InspectGetKeyKind::HeaderReplicationTimestamp,
        InspectGetNeed::Header,
    ),
    ("indexed", InspectGetKeyKind::Indexed, InspectGetNeed::Scan),
    (
        "blocks.total",
        InspectGetKeyKind::BlocksTotal,
        InspectGetNeed::Scan,
    ),
    (
        "elements.nodes",
        InspectGetKeyKind::ElementsNodes,
        InspectGetNeed::Scan,
    ),
    (
        "elements.ways",
        InspectGetKeyKind::ElementsWays,
        InspectGetNeed::Scan,
    ),
    (
        "elements.relations",
        InspectGetKeyKind::ElementsRelations,
        InspectGetNeed::Scan,
    ),
    (
        "elements.total",
        InspectGetKeyKind::ElementsTotal,
        InspectGetNeed::Scan,
    ),
    (
        "data.objects_ordered",
        InspectGetKeyKind::DataObjectsOrdered,
        InspectGetNeed::Extended,
    ),
    (
        "data.timestamp.first",
        InspectGetKeyKind::DataTimestampFirst,
        InspectGetNeed::Extended,
    ),
    (
        "data.timestamp.last",
        InspectGetKeyKind::DataTimestampLast,
        InspectGetNeed::Extended,
    ),
    (
        "data.bbox",
        InspectGetKeyKind::DataBbox,
        InspectGetNeed::Extended,
    ),
    (
        "data.count.nodes",
        InspectGetKeyKind::DataCountNodes,
        InspectGetNeed::Extended,
    ),
    (
        "data.count.ways",
        InspectGetKeyKind::DataCountWays,
        InspectGetNeed::Extended,
    ),
    (
        "data.count.relations",
        InspectGetKeyKind::DataCountRelations,
        InspectGetNeed::Extended,
    ),
    (
        "metadata.all_objects.version",
        InspectGetKeyKind::MetadataAllObjectsVersion,
        InspectGetNeed::Extended,
    ),
    (
        "metadata.all_objects.timestamp",
        InspectGetKeyKind::MetadataAllObjectsTimestamp,
        InspectGetNeed::Extended,
    ),
    (
        "metadata.all_objects.changeset",
        InspectGetKeyKind::MetadataAllObjectsChangeset,
        InspectGetNeed::Extended,
    ),
    (
        "metadata.all_objects.uid",
        InspectGetKeyKind::MetadataAllObjectsUid,
        InspectGetNeed::Extended,
    ),
    (
        "metadata.all_objects.user",
        InspectGetKeyKind::MetadataAllObjectsUser,
        InspectGetNeed::Extended,
    ),
    (
        "metadata.some_objects.version",
        InspectGetKeyKind::MetadataSomeObjectsVersion,
        InspectGetNeed::Extended,
    ),
    (
        "metadata.some_objects.timestamp",
        InspectGetKeyKind::MetadataSomeObjectsTimestamp,
        InspectGetNeed::Extended,
    ),
    (
        "metadata.some_objects.changeset",
        InspectGetKeyKind::MetadataSomeObjectsChangeset,
        InspectGetNeed::Extended,
    ),
    (
        "metadata.some_objects.uid",
        InspectGetKeyKind::MetadataSomeObjectsUid,
        InspectGetNeed::Extended,
    ),
    (
        "metadata.some_objects.user",
        InspectGetKeyKind::MetadataSomeObjectsUser,
        InspectGetNeed::Extended,
    ),
];

impl InspectGetKey {
    /// Parse a supported `inspect --get` path.
    #[must_use]
    pub(super) fn parse(key: &str) -> Option<Self> {
        INSPECT_GET_KEYS
            .iter()
            .find(|(name, _, _)| *name == key)
            .map(|(_, kind, need)| Self {
                kind: *kind,
                need: *need,
            })
    }

    /// Return the value represented by this path.
    #[must_use]
    pub(super) const fn kind(self) -> InspectGetKeyKind {
        self.kind
    }

    /// Return the work needed to produce this path's value.
    #[must_use]
    pub(super) const fn need(self) -> InspectGetNeed {
        self.need
    }
}

/// A parsed get path and its scan requirement.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InspectGetLookup {
    kind: Option<InspectGetKeyKind>,
    need: InspectGetNeed,
}

impl InspectGetLookup {
    /// Return the value kind when the path is recognized.
    #[must_use]
    pub const fn kind(self) -> Option<InspectGetKeyKind> {
        self.kind
    }

    /// Return the work needed to resolve the path.
    #[must_use]
    pub const fn need(self) -> InspectGetNeed {
        self.need
    }
}

/// Parse a get path and report its scan requirement, including the historical
/// prefix routing used to report unknown `data.*`, `metadata.*`, `elements.*`
/// and `blocks.*` paths from the matching report mode.
#[doc(hidden)]
#[must_use]
pub fn inspect_get_lookup(key: &str) -> InspectGetLookup {
    if let Some(known) = InspectGetKey::parse(key) {
        return InspectGetLookup {
            kind: Some(known.kind()),
            need: known.need(),
        };
    }
    let need = if key.starts_with("data.") || key.starts_with("metadata.") {
        InspectGetNeed::Extended
    } else if key.starts_with("elements.") || key.starts_with("blocks.") {
        InspectGetNeed::Scan
    } else {
        InspectGetNeed::Header
    };
    InspectGetLookup { kind: None, need }
}
