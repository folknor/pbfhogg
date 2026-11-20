//! Shared protobuf field numbers for PBF envelopes and header metadata.

pub(crate) mod blob {
    pub(crate) const RAW: u32 = 1;
    pub(crate) const RAW_SIZE: u32 = 2;
    pub(crate) const ZLIB_DATA: u32 = 3;
    /// Payload alternatives in the schema's oneof that pbfhogg cannot decode.
    pub(crate) const LZMA_DATA: u32 = 4;
    pub(crate) const OBSOLETE_BZIP2_DATA: u32 = 5;
    pub(crate) const LZ4_DATA: u32 = 6;
    pub(crate) const ZSTD_DATA: u32 = 7;
}

/// A field's identity on the wire: its number and its wire type together.
/// A reader recognises a field only by the pair (an occurrence of a known
/// number with another wire type is an unknown field), and a verbatim
/// rewriter removes exactly the occurrences the reader would recognise, so
/// both take the pair from here.
pub(crate) type Tag = (u32, u32);

pub(crate) mod blob_header {
    use super::Tag;
    use protohoggr::{WIRE_LEN, WIRE_VARINT};

    pub(crate) const TYPE: u32 = 1;
    pub(crate) const INDEXDATA: u32 = 2;
    pub(crate) const DATASIZE: u32 = 3;
    pub(crate) const TAGDATA: u32 = 4;
    pub(crate) const WAYMEMBERS: u32 = 5;

    pub(crate) const TYPE_TAG: Tag = (TYPE, WIRE_LEN);
    pub(crate) const INDEXDATA_TAG: Tag = (INDEXDATA, WIRE_LEN);
    pub(crate) const DATASIZE_TAG: Tag = (DATASIZE, WIRE_VARINT);
    pub(crate) const TAGDATA_TAG: Tag = (TAGDATA, WIRE_LEN);
    pub(crate) const WAYMEMBERS_TAG: Tag = (WAYMEMBERS, WIRE_LEN);

    /// The schema wire type of a known BlobHeader field number, `None` for
    /// a number the schema does not define.
    pub(crate) const fn wire_type(field: u32) -> Option<u32> {
        match field {
            TYPE => Some(TYPE_TAG.1),
            INDEXDATA => Some(INDEXDATA_TAG.1),
            DATASIZE => Some(DATASIZE_TAG.1),
            TAGDATA => Some(TAGDATA_TAG.1),
            WAYMEMBERS => Some(WAYMEMBERS_TAG.1),
            _ => None,
        }
    }
}

pub(crate) mod header_block {
    use super::Tag;
    use protohoggr::{WIRE_LEN, WIRE_VARINT};

    pub(crate) const BBOX: u32 = 1;
    pub(crate) const REQUIRED_FEATURES: u32 = 4;
    pub(crate) const OPTIONAL_FEATURES: u32 = 5;
    pub(crate) const WRITING_PROGRAM: u32 = 16;
    pub(crate) const SOURCE: u32 = 17;
    pub(crate) const REPLICATION_TIMESTAMP: u32 = 32;
    pub(crate) const REPLICATION_SEQUENCE_NUMBER: u32 = 33;
    pub(crate) const REPLICATION_BASE_URL: u32 = 34;

    pub(crate) const BBOX_TAG: Tag = (BBOX, WIRE_LEN);
    pub(crate) const REQUIRED_FEATURES_TAG: Tag = (REQUIRED_FEATURES, WIRE_LEN);
    pub(crate) const OPTIONAL_FEATURES_TAG: Tag = (OPTIONAL_FEATURES, WIRE_LEN);
    pub(crate) const WRITING_PROGRAM_TAG: Tag = (WRITING_PROGRAM, WIRE_LEN);
    pub(crate) const SOURCE_TAG: Tag = (SOURCE, WIRE_LEN);
    pub(crate) const REPLICATION_TIMESTAMP_TAG: Tag = (REPLICATION_TIMESTAMP, WIRE_VARINT);
    pub(crate) const REPLICATION_SEQUENCE_NUMBER_TAG: Tag =
        (REPLICATION_SEQUENCE_NUMBER, WIRE_VARINT);
    pub(crate) const REPLICATION_BASE_URL_TAG: Tag = (REPLICATION_BASE_URL, WIRE_LEN);
}

pub(crate) mod header_bbox {
    pub(crate) const LEFT: u32 = 1;
    pub(crate) const RIGHT: u32 = 2;
    pub(crate) const TOP: u32 = 3;
    pub(crate) const BOTTOM: u32 = 4;
}

pub(crate) mod string_table {
    pub(crate) const STRINGS: u32 = 1;
}

pub(crate) mod primitive_block {
    pub(crate) const STRING_TABLE: u32 = 1;
}

/// BlobHeader fields a header-preserving reframe strips before writing its
/// own (degrade's metadata-only path, `cat --reindex`, `cat --repair`).
#[derive(Clone, Copy)]
pub(crate) enum BlobHeaderField {
    Indexdata,
    /// Replaced, never only removed: the frame gets a new body.
    Datasize,
    Tagdata,
}

impl BlobHeaderField {
    /// The (number, wire type) pair a strip removes: exactly the
    /// occurrences `WireBlobHeader::parse` reads as this field.
    pub(crate) const fn tag(self) -> Tag {
        match self {
            Self::Indexdata => blob_header::INDEXDATA_TAG,
            Self::Datasize => blob_header::DATASIZE_TAG,
            Self::Tagdata => blob_header::TAGDATA_TAG,
        }
    }
}

/// HeaderBlock fields that degrade's verbatim header passthrough can remove
/// (the bbox under `--strip-bbox`) or replace (the ones `--generator` and
/// `--output-header` override).
#[derive(Clone, Copy)]
pub(crate) enum HeaderBlockField {
    Bbox,
    WritingProgram,
    ReplicationTimestamp,
    ReplicationSequenceNumber,
    ReplicationBaseUrl,
}

impl HeaderBlockField {
    /// The (number, wire type) pair a strip removes: exactly the
    /// occurrences `WireHeaderBlock::parse` reads as this field.
    pub(crate) const fn tag(self) -> Tag {
        match self {
            Self::Bbox => header_block::BBOX_TAG,
            Self::WritingProgram => header_block::WRITING_PROGRAM_TAG,
            Self::ReplicationTimestamp => header_block::REPLICATION_TIMESTAMP_TAG,
            Self::ReplicationSequenceNumber => header_block::REPLICATION_SEQUENCE_NUMBER_TAG,
            Self::ReplicationBaseUrl => header_block::REPLICATION_BASE_URL_TAG,
        }
    }
}
