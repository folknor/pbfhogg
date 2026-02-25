//! Display PBF file metadata. Equivalent to `osmium fileinfo`.

use std::path::Path;

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
}

/// Read PBF metadata. If `extended`, do a full scan to count blobs and elements.
#[hotpath::measure]
pub fn fileinfo(path: &Path, extended: bool, direct_io: bool) -> Result<FileInfo> {
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
    };

    let mut blobs: u64 = 0;
    let mut nodes: u64 = 0;
    let mut ways: u64 = 0;
    let mut relations: u64 = 0;

    for blob in reader {
        let blob = blob?;
        match blob.decode()? {
            BlobDecode::OsmHeader(header) => {
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

                if !extended {
                    break;
                }
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

    if extended {
        info.blob_count = Some(blobs);
        info.node_count = Some(nodes);
        info.way_count = Some(ways);
        info.relation_count = Some(relations);
    }

    Ok(info)
}
