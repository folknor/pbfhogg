//! PBF file writer — blob framing and compression.
//!
//! Writes valid `.osm.pbf` files. The writer handles the low-level blob framing
//! (4-byte header length, BlobHeader, compressed Blob) and delegates block
//! construction to [`BlockBuilder`](crate::block_builder::BlockBuilder).

use crate::proto::fileformat;
use bytes::Bytes;
use flate2::write::ZlibEncoder;
use flate2::Compression as FlateCompression;
use protobuf::Message;
use std::io::{self, Write};
use std::path::Path;

/// Compression algorithm for PBF output blobs.
pub enum Compression {
    /// No compression (raw bytes).
    None,
    /// Zlib compression at the given level (0–9).
    /// Level 6 matches osmium's default (`Z_DEFAULT_COMPRESSION`).
    Zlib(u32),
}

impl Default for Compression {
    fn default() -> Self {
        Compression::Zlib(6)
    }
}

/// Writes PBF files as a sequence of framed, compressed blobs.
///
/// # Usage
///
/// 1. Call [`write_header`](Self::write_header) with a serialized `HeaderBlock`.
/// 2. Call [`write_primitive_block`](Self::write_primitive_block) for each data block.
/// 3. Call [`flush`](Self::flush) when done.
///
/// For merge passthrough, use [`write_raw`](Self::write_raw) to copy unmodified
/// blob bytes directly.
pub struct PbfWriter<W: Write> {
    writer: W,
    compression: Compression,
}

impl PbfWriter<io::BufWriter<std::fs::File>> {
    /// Create a `PbfWriter` that writes to a file at the given path.
    pub fn to_path(path: &Path, compression: Compression) -> io::Result<Self> {
        let file = std::fs::File::create(path)?;
        let writer = io::BufWriter::new(file);
        Ok(PbfWriter {
            writer,
            compression,
        })
    }
}

impl<W: Write> PbfWriter<W> {
    /// Create a new `PbfWriter` wrapping the given writer.
    pub fn new(writer: W, compression: Compression) -> Self {
        PbfWriter {
            writer,
            compression,
        }
    }

    /// Write the `OSMHeader` blob. Must be the first blob in the file.
    ///
    /// `header_block_bytes` is a serialized `HeaderBlock` protobuf message,
    /// typically produced by [`build_header`](crate::block_builder::build_header).
    pub fn write_header(&mut self, header_block_bytes: &[u8]) -> io::Result<()> {
        self.write_blob("OSMHeader", header_block_bytes)
    }

    /// Write an `OSMData` blob from a serialized `PrimitiveBlock`.
    ///
    /// `block_bytes` is produced by [`BlockBuilder::take`](crate::block_builder::BlockBuilder::take).
    pub fn write_primitive_block(&mut self, block_bytes: &[u8]) -> io::Result<()> {
        self.write_blob("OSMData", block_bytes)
    }

    /// Write pre-framed raw blob bytes directly to the output.
    ///
    /// Used for passthrough of unaffected blocks during merge.
    /// The caller is responsible for providing valid framed bytes:
    /// `[4-byte BE header_len][BlobHeader][Blob]`.
    pub fn write_raw(&mut self, raw_framed_bytes: &[u8]) -> io::Result<()> {
        self.writer.write_all(raw_framed_bytes)
    }

    /// Flush the underlying writer.
    pub fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }

    /// Consume the writer and return the inner writer.
    pub fn into_inner(self) -> W {
        self.writer
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    fn write_blob(&mut self, blob_type: &str, uncompressed: &[u8]) -> io::Result<()> {
        // Step 1: Build the Blob protobuf (optionally compressed)
        let mut blob = fileformat::Blob::new();
        match self.compression {
            Compression::None => {
                blob.data = Some(fileformat::blob::Data::Raw(Bytes::copy_from_slice(
                    uncompressed,
                )));
            }
            Compression::Zlib(level) => {
                let mut encoder = ZlibEncoder::new(Vec::new(), FlateCompression::new(level));
                encoder.write_all(uncompressed)?;
                let compressed = encoder.finish()?;
                blob.set_raw_size(uncompressed.len() as i32);
                blob.data = Some(fileformat::blob::Data::ZlibData(Bytes::from(compressed)));
            }
        }

        let blob_bytes = blob
            .write_to_bytes()
            .map_err(io::Error::other)?;

        // Step 2: Build the BlobHeader
        let mut header = fileformat::BlobHeader::new();
        header.set_type(protobuf::Chars::from(blob_type));
        header.set_datasize(blob_bytes.len() as i32);

        let header_bytes = header
            .write_to_bytes()
            .map_err(io::Error::other)?;

        // Step 3: Write [4-byte BE header_len][BlobHeader][Blob]
        let header_len = header_bytes.len() as u32;
        self.writer.write_all(&header_len.to_be_bytes())?;
        self.writer.write_all(&header_bytes)?;
        self.writer.write_all(&blob_bytes)?;

        Ok(())
    }
}
