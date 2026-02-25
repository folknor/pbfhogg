//! Concatenate PBF files with optional type filtering. Equivalent to `osmium cat`.

use std::io::{self, Read};
use std::path::Path;

use super::{flush_block, rebuild_header};
use crate::block_builder::{BlockBuilder, MemberData, Metadata};
use crate::blob::{decode_blob_to_headerblock, parse_blob_header};
use crate::file_reader::FileReader;
use crate::writer::{Compression, PbfWriter};
use crate::{BlobDecode, BlobReader, Element};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Statistics from a cat operation.
pub struct CatStats {
    pub blobs_passthrough: u64,
    pub blobs_decoded: u64,
    pub elements_written: u64,
}

impl CatStats {
    pub fn print_summary(&self) {
        if self.blobs_decoded > 0 {
            eprintln!(
                "Decoded {} blobs, wrote {} elements",
                self.blobs_decoded, self.elements_written,
            );
        } else {
            eprintln!("{} blobs passed through", self.blobs_passthrough);
        }
    }
}

/// Concatenate one or more PBF files into a single output.
///
/// If `type_filter` is set (comma-separated: "node", "way", "relation"),
/// only elements of matching types are included (requires full decode).
/// Without a filter, blobs are passed through as raw bytes (zero decode).
#[hotpath::measure]
pub fn cat(
    files: &[&Path],
    output: &Path,
    type_filter: Option<&str>,
    compression: Compression,
    direct_io: bool,
) -> Result<CatStats> {
    match type_filter {
        None => cat_passthrough(files, output, compression, direct_io),
        Some(filter) => cat_filtered(files, output, filter, compression, direct_io),
    }
}

// ---------------------------------------------------------------------------
// Passthrough path: no type filter, zero decode
// ---------------------------------------------------------------------------

/// Raw blob frame: complete framed bytes for write_raw() passthrough.
struct RawBlobFrame {
    frame_bytes: Vec<u8>,
    blob_type: String,
    blob_bytes: Vec<u8>,
    /// Byte offset of this frame in the input file (for copy_file_range).
    #[cfg_attr(not(feature = "linux-direct-io"), allow(dead_code))]
    file_offset: u64,
}

/// Read the next raw blob frame. Returns None at EOF.
/// Updates `file_offset` to track position for copy_file_range.
fn read_raw_frame<R: Read>(
    reader: &mut R,
    file_offset: &mut u64,
) -> Result<Option<RawBlobFrame>> {
    let frame_start = *file_offset;

    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    #[allow(clippy::cast_possible_truncation)]
    let header_len = u32::from_be_bytes(len_buf) as usize;

    let mut header_bytes = vec![0u8; header_len];
    reader.read_exact(&mut header_bytes)?;
    let (blob_type, data_size) = parse_blob_header(&header_bytes)?;

    let mut blob_bytes = vec![0u8; data_size];
    reader.read_exact(&mut blob_bytes)?;

    let frame_len = 4 + header_len + data_size;
    *file_offset += frame_len as u64;
    let mut frame_bytes = Vec::with_capacity(frame_len);
    frame_bytes.extend_from_slice(&len_buf);
    frame_bytes.extend_from_slice(&header_bytes);
    frame_bytes.extend_from_slice(&blob_bytes);

    Ok(Some(RawBlobFrame {
        frame_bytes,
        blob_type,
        blob_bytes,
        file_offset: frame_start,
    }))
}

fn cat_passthrough(files: &[&Path], output: &Path, compression: Compression, direct_io: bool) -> Result<CatStats> {
    let mut writer = PbfWriter::to_path(output, compression)?;
    let mut header_written = false;
    let mut blobs: u64 = 0;

    for file in files {
        let mut reader = FileReader::open(file, direct_io)?;
        let mut file_offset: u64 = 0;

        // copy_file_range: writer is always buffered in cat, so always safe.
        // O_DIRECT input + copy_file_range works (explicit offset, kernel reads).
        #[cfg(feature = "linux-direct-io")]
        let input_fd = reader.raw_fd();

        while let Some(frame) = read_raw_frame(&mut reader, &mut file_offset)? {
            match frame.blob_type.as_str() {
                "OSMHeader" => {
                    if !header_written {
                        let header = decode_blob_to_headerblock(&frame.blob_bytes)?;
                        rebuild_header(&header, &mut writer)?;
                        header_written = true;
                    }
                }
                "OSMData" => {
                    #[cfg(feature = "linux-direct-io")]
                    writer.write_raw_copy(
                        input_fd,
                        frame.file_offset,
                        frame.frame_bytes.len() as u64,
                    )?;
                    #[cfg(not(feature = "linux-direct-io"))]
                    writer.write_raw(&frame.frame_bytes)?;
                    blobs += 1;
                }
                _ => {}
            }
        }
    }

    writer.flush()?;
    Ok(CatStats {
        blobs_passthrough: blobs,
        blobs_decoded: 0,
        elements_written: 0,
    })
}

// ---------------------------------------------------------------------------
// Filtered path: decode + rebuild
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
fn cat_filtered(files: &[&Path], output: &Path, filter: &str, compression: Compression, direct_io: bool) -> Result<CatStats> {
    let filter_node = filter.split(',').any(|t| t.trim() == "node");
    let filter_way = filter.split(',').any(|t| t.trim() == "way");
    let filter_relation = filter.split(',').any(|t| t.trim() == "relation");

    let mut writer = PbfWriter::to_path(output, compression)?;
    let mut bb = BlockBuilder::new();
    let mut header_written = false;
    let mut blobs_decoded: u64 = 0;
    let mut elements: u64 = 0;

    for file in files {
        let reader = BlobReader::open(file, direct_io)?;

        for blob in reader {
            let blob = blob?;
            match blob.decode()? {
                BlobDecode::OsmHeader(header) => {
                    if !header_written {
                        rebuild_header(&header, &mut writer)?;
                        header_written = true;
                    }
                }
                BlobDecode::OsmData(block) => {
                    blobs_decoded += 1;

                    // Reusable buffers for element data, hoisted outside the element loop.
                    //
                    // WHY: Without hoisting, each element allocates fresh Vecs via .collect(),
                    // producing N allocations where N = number of elements. For Denmark (~50M
                    // elements), that is ~150M alloc/dealloc pairs across the 3 buffer types.
                    //
                    // HOW: Vec::clear() sets len to 0 but keeps the underlying heap allocation.
                    // The subsequent extend() refills the buffer without reallocating once the
                    // capacity is warm (i.e. after the first few elements in each block).
                    //
                    // These buffers grow to the size of the largest element in the block and
                    // stabilize — there is no unbounded growth because PBF blocks have a max
                    // of 8000 entities. They are scoped to the OsmData arm so that the borrowed
                    // string references (which point into `block`) do not outlive the block.
                    let mut tags_buf: Vec<(&str, &str)> = Vec::new();
                    let mut refs_buf: Vec<i64> = Vec::new();
                    let mut members_buf: Vec<MemberData<'_>> = Vec::new();

                    for element in block.elements() {
                        match &element {
                            Element::DenseNode(dn) if filter_node => {
                                if !bb.can_add_node() {
                                    flush_block(&mut bb, &mut writer)?;
                                }
                                tags_buf.clear();
                                tags_buf.extend(dn.tags());
                                let meta = dn.info().and_then(|info| {
                                    let user = info.user().ok()?;
                                    Some(Metadata {
                                        version: info.version(),
                                        timestamp: info.milli_timestamp() / 1000,
                                        changeset: info.changeset(),
                                        uid: info.uid(),
                                        user,
                                        visible: info.visible(),
                                    })
                                });
                                bb.add_node(
                                    dn.id(),
                                    dn.decimicro_lat(),
                                    dn.decimicro_lon(),
                                    &tags_buf,
                                    meta.as_ref(),
                                );
                                elements += 1;
                            }
                            Element::Node(n) if filter_node => {
                                if !bb.can_add_node() {
                                    flush_block(&mut bb, &mut writer)?;
                                }
                                tags_buf.clear();
                                tags_buf.extend(n.tags());
                                let info = n.info();
                                let meta = info.version().map(|v| Metadata {
                                    version: v,
                                    timestamp: info.milli_timestamp().unwrap_or(0) / 1000,
                                    changeset: info.changeset().unwrap_or(0),
                                    uid: info.uid().unwrap_or(0),
                                    user: info
                                        .user()
                                        .and_then(std::result::Result::ok)
                                        .unwrap_or(""),
                                    visible: info.visible(),
                                });
                                bb.add_node(
                                    n.id(),
                                    n.decimicro_lat(),
                                    n.decimicro_lon(),
                                    &tags_buf,
                                    meta.as_ref(),
                                );
                                elements += 1;
                            }
                            Element::Way(w) if filter_way => {
                                if !bb.can_add_way() {
                                    flush_block(&mut bb, &mut writer)?;
                                }
                                tags_buf.clear();
                                tags_buf.extend(w.tags());
                                refs_buf.clear();
                                refs_buf.extend(w.refs());
                                let info = w.info();
                                let meta = info.version().map(|v| Metadata {
                                    version: v,
                                    timestamp: info.milli_timestamp().unwrap_or(0) / 1000,
                                    changeset: info.changeset().unwrap_or(0),
                                    uid: info.uid().unwrap_or(0),
                                    user: info
                                        .user()
                                        .and_then(std::result::Result::ok)
                                        .unwrap_or(""),
                                    visible: info.visible(),
                                });
                                bb.add_way(w.id(), &tags_buf, &refs_buf, meta.as_ref());
                                elements += 1;
                            }
                            Element::Relation(r) if filter_relation => {
                                if !bb.can_add_relation() {
                                    flush_block(&mut bb, &mut writer)?;
                                }
                                tags_buf.clear();
                                tags_buf.extend(r.tags());
                                members_buf.clear();
                                members_buf.extend(r.members().map(|m| MemberData {
                                    id: m.id,
                                    role: m.role().unwrap_or(""),
                                }));
                                let info = r.info();
                                let meta = info.version().map(|v| Metadata {
                                    version: v,
                                    timestamp: info.milli_timestamp().unwrap_or(0) / 1000,
                                    changeset: info.changeset().unwrap_or(0),
                                    uid: info.uid().unwrap_or(0),
                                    user: info
                                        .user()
                                        .and_then(std::result::Result::ok)
                                        .unwrap_or(""),
                                    visible: info.visible(),
                                });
                                bb.add_relation(r.id(), &tags_buf, &members_buf, meta.as_ref());
                                elements += 1;
                            }
                            _ => {}
                        }
                    }
                }
                BlobDecode::Unknown(_) => {}
            }
        }
    }

    flush_block(&mut bb, &mut writer)?;
    writer.flush()?;

    Ok(CatStats {
        blobs_passthrough: 0,
        blobs_decoded,
        elements_written: elements,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

