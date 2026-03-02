//! Display PBF file metadata. Equivalent to `osmium fileinfo`.

use std::path::Path;

use super::read_raw_frame;
use crate::blob::{decode_blob_to_headerblock, BlobKind};
use crate::blob_index::ElemKind;
use crate::file_reader::FileReader;
use crate::{BlobDecode, BlobReader, Element};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Metadata extracted from a PBF file.
pub struct FileInfo {
    pub bbox: Option<(f64, f64, f64, f64)>,
    pub writing_program: Option<String>,
    pub required_features: Vec<String>,
    pub optional_features: Vec<String>,
    pub replication_timestamp: Option<i64>,
    pub replication_sequence: Option<i64>,
    pub replication_url: Option<String>,
    pub blob_count: Option<u64>,
    pub node_count: Option<u64>,
    pub way_count: Option<u64>,
    pub relation_count: Option<u64>,
    pub is_indexed: Option<bool>,
}

/// Read PBF metadata. If `extended`, do a full scan to count blobs and elements.
///
/// When the file has blob-level indexdata, the extended scan reads only blob
/// headers (no decompression) — typically 100x+ faster than the fallback path.
#[hotpath::measure]
pub fn fileinfo(path: &Path, extended: bool, direct_io: bool) -> Result<FileInfo> {
    if extended {
        fileinfo_extended(path, direct_io)
    } else {
        fileinfo_header_only(path, direct_io)
    }
}

/// Header-only scan: read the OsmHeader blob and return metadata.
fn fileinfo_header_only(path: &Path, direct_io: bool) -> Result<FileInfo> {
    let reader = BlobReader::open(path, direct_io)?;
    let mut info = FileInfo {
        bbox: None,
        writing_program: None,
        required_features: Vec::new(),
        optional_features: Vec::new(),
        replication_timestamp: None,
        replication_sequence: None,
        replication_url: None,
        blob_count: None,
        node_count: None,
        way_count: None,
        relation_count: None,
        is_indexed: None,
    };

    for blob in reader {
        let blob = blob?;
        if let BlobDecode::OsmHeader(header) = blob.decode()? {
            fill_header_metadata(&mut info, &header);
            break;
        }
    }

    Ok(info)
}

/// Extended scan: count blobs and elements. Uses the fast path (blob header
/// indexdata) when available, falls back to full decompression otherwise.
fn fileinfo_extended(path: &Path, direct_io: bool) -> Result<FileInfo> {
    let mut reader = FileReader::open(path, direct_io)?;
    let mut offset = 0u64;
    let mut info = FileInfo {
        bbox: None,
        writing_program: None,
        required_features: Vec::new(),
        optional_features: Vec::new(),
        replication_timestamp: None,
        replication_sequence: None,
        replication_url: None,
        blob_count: None,
        node_count: None,
        way_count: None,
        relation_count: None,
        is_indexed: None,
    };

    let mut blobs: u64 = 0;
    let mut nodes: u64 = 0;
    let mut ways: u64 = 0;
    let mut relations: u64 = 0;
    let mut indexed = true;

    while let Some(frame) = read_raw_frame(&mut reader, &mut offset)? {
        match frame.blob_type {
            BlobKind::OsmHeader => {
                let header = decode_blob_to_headerblock(frame.blob_bytes())?;
                fill_header_metadata(&mut info, &header);
            }
            BlobKind::OsmData => {
                blobs += 1;
                if let Some(ref idx) = frame.index {
                    match idx.kind {
                        ElemKind::Node => nodes += idx.count,
                        ElemKind::Way => ways += idx.count,
                        ElemKind::Relation => relations += idx.count,
                    }
                } else {
                    indexed = false;
                    break;
                }
            }
            BlobKind::Unknown(_) => {}
        }
    }

    if !indexed {
        // First unindexed blob found — fall back to full decode for remainder.
        // Re-open and do a full scan from the beginning (simpler than resuming
        // mid-file, and non-indexed files are already slow).
        return fileinfo_extended_slow(path, direct_io);
    }

    info.blob_count = Some(blobs);
    info.node_count = Some(nodes);
    info.way_count = Some(ways);
    info.relation_count = Some(relations);
    info.is_indexed = Some(true);

    Ok(info)
}

/// Full-decode fallback for files without indexdata.
fn fileinfo_extended_slow(path: &Path, direct_io: bool) -> Result<FileInfo> {
    let reader = BlobReader::open(path, direct_io)?;
    let mut info = FileInfo {
        bbox: None,
        writing_program: None,
        required_features: Vec::new(),
        optional_features: Vec::new(),
        replication_timestamp: None,
        replication_sequence: None,
        replication_url: None,
        blob_count: None,
        node_count: None,
        way_count: None,
        relation_count: None,
        is_indexed: Some(false),
    };

    let mut blobs: u64 = 0;
    let mut nodes: u64 = 0;
    let mut ways: u64 = 0;
    let mut relations: u64 = 0;

    for blob in reader {
        let blob = blob?;
        match blob.decode()? {
            BlobDecode::OsmHeader(header) => {
                fill_header_metadata(&mut info, &header);
            }
            BlobDecode::OsmData(block) => {
                blobs += 1;
                for element in block.elements() {
                    match element {
                        Element::DenseNode(_) | Element::Node(_) => nodes += 1,
                        Element::Way(_) => ways += 1,
                        Element::Relation(_) => relations += 1,
                    }
                }
            }
            BlobDecode::Unknown(_) => {}
        }
    }

    info.blob_count = Some(blobs);
    info.node_count = Some(nodes);
    info.way_count = Some(ways);
    info.relation_count = Some(relations);

    Ok(info)
}

fn fill_header_metadata(info: &mut FileInfo, header: &crate::HeaderBlock) {
    if let Some(bb) = header.bbox() {
        info.bbox = Some((bb.left, bb.bottom, bb.right, bb.top));
    }
    info.writing_program = header.writing_program().map(String::from);
    info.required_features = header
        .required_features()
        .iter()
        .map(ToString::to_string)
        .collect();
    info.optional_features = header
        .optional_features()
        .iter()
        .map(ToString::to_string)
        .collect();
    info.replication_timestamp = header.osmosis_replication_timestamp();
    info.replication_sequence = header.osmosis_replication_sequence_number();
    info.replication_url = header
        .osmosis_replication_base_url()
        .map(String::from);
}
