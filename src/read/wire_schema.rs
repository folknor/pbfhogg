//! Shared protobuf field numbers for PBF envelopes and header metadata.

pub(crate) mod blob {
    pub(crate) const RAW: u32 = 1;
    pub(crate) const RAW_SIZE: u32 = 2;
    pub(crate) const ZLIB_DATA: u32 = 3;
    pub(crate) const ZSTD_DATA: u32 = 7;
}

pub(crate) mod blob_header {
    pub(crate) const TYPE: u32 = 1;
    pub(crate) const INDEXDATA: u32 = 2;
    pub(crate) const DATASIZE: u32 = 3;
    pub(crate) const TAGDATA: u32 = 4;
    pub(crate) const WAYMEMBERS: u32 = 5;
}

pub(crate) mod header_block {
    pub(crate) const BBOX: u32 = 1;
    pub(crate) const REQUIRED_FEATURES: u32 = 4;
    pub(crate) const OPTIONAL_FEATURES: u32 = 5;
    pub(crate) const WRITING_PROGRAM: u32 = 16;
    pub(crate) const SOURCE: u32 = 17;
    pub(crate) const REPLICATION_TIMESTAMP: u32 = 32;
    pub(crate) const REPLICATION_SEQUENCE_NUMBER: u32 = 33;
    pub(crate) const REPLICATION_BASE_URL: u32 = 34;
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

/// BlobHeader fields that the metadata-only degrade path can remove.
#[derive(Clone, Copy)]
#[repr(u32)]
pub(crate) enum BlobHeaderField {
    Indexdata = blob_header::INDEXDATA,
    Tagdata = blob_header::TAGDATA,
}

/// HeaderBlock fields that the metadata-only degrade path can remove.
#[derive(Clone, Copy)]
#[repr(u32)]
pub(crate) enum HeaderBlockField {
    Bbox = header_block::BBOX,
}
