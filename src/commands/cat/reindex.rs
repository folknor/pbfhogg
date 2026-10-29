//! `cat --reindex`: rewrite every data blob's indexdata and tagdata from its
//! payload, losslessly.
//!
//! The repair for a file whose blob indexdata is false. Plain `cat` passes an
//! indexed blob through with its claim, and `--clean` re-encodes every block,
//! normalising what it decodes and stripping an attribute, so neither repairs
//! a false claim without changing anything else. This does: each OSMData
//! blob's compressed payload is copied byte for byte, and its `BlobHeader`
//! keeps every field (the `WayMembers-v1` payload in field 5 and unknown
//! fields included) except `indexdata` (field 2) and `tagdata` (field 4),
//! every occurrence of which is removed and replaced by one scanned from the
//! payload. The OSMHeader's `HeaderBlock` is carried through unchanged (its
//! frame is written afresh by the output writer, under the run's
//! compression), and every other blob is copied as it is.
//!
//! A block no single index can describe (no elements, a group carrying more
//! than one element field, or kinds mixed across groups) is refused: giving
//! it no indexdata would leave a partially indexed file every command
//! refuses, and re-encoding it, as plain `cat` does, is not lossless.

use crate::blob::{BlobKind, decompress_blob_data_into};
use crate::error::{Error, Result};
use crate::file_reader::FileReader;
use crate::read::frame::Frames;
use crate::read::input::InputSource;
use crate::read::wire_schema::BlobHeaderField;
use crate::write::WriteOptions;

/// What a reindex run did.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReindexStats {
    /// OSMData blobs written, each with indexdata scanned from its payload.
    pub blobs: u64,
    /// Of those, the blobs whose indexdata the input lacked or stated
    /// differently.
    pub blobs_changed: u64,
}

impl ReindexStats {
    /// The one-line summary.
    pub fn write_summary(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        writeln!(
            w,
            "Reindexed {} blobs; {} carried indexdata that was missing or disagreed with \
             the payload",
            self.blobs, self.blobs_changed
        )
    }
}

/// Rewrite `input`'s indexdata and tagdata from its payloads into `output`.
/// See the module documentation for what is kept and what is refused.
///
/// # Errors
/// Besides read, decompression and write failures: a data blob whose block
/// no single index can describe, and a rebuilt `BlobHeader` that would reach
/// the format's header-size limit even without its tagdata.
pub fn reindex<'a>(
    input: impl Into<InputSource<'a>>,
    output: impl crate::output::BindOutput,
    write: &WriteOptions,
    host: &crate::host::Host,
) -> Result<ReindexStats> {
    let output = output.bind_to(&host.invocation)?;
    host.invocation.check()?;
    let stats = reindex_to(input.into(), &output, write, host)?;
    output.publish()?;
    Ok(stats)
}

fn reindex_to(
    source: InputSource<'_>,
    output: &crate::output::BoundOutput,
    write: &WriteOptions,
    host: &crate::host::Host,
) -> Result<ReindexStats> {
    let path = source.path();
    let mut frames = Frames::of_input(
        FileReader::open_in(source, write.direct_reads, &host.invocation)?,
        path,
        host,
    );
    // The remedy for a file whose indexdata cannot be believed, a partially
    // indexed one included: every blob is rescanned whatever it carries.
    frames.accept_partial_index();

    let header = frames
        .next_raw()?
        .ok_or_else(|| crate::error::new_missing_header(path))?;
    if !matches!(header.blob_type, BlobKind::OsmHeader) {
        return Err(crate::error::new_missing_header(path));
    }
    let mut header_block = Vec::new();
    decompress_blob_data_into(header.blob_bytes(), &mut header_block)
        .map_err(|e| frames.locate(header.id, e))?;
    let mut writer = super::super::writer_from_header_bytes(
        output,
        write.compression,
        &header_block,
        write.backend,
        host,
    )?;

    let mut stats = ReindexStats::default();
    let mut payload: Vec<u8> = Vec::new();
    let mut header_buf: Vec<u8> = Vec::new();
    while let Some(mut frame) = frames.next_raw()? {
        if !matches!(frame.blob_type, BlobKind::OsmData) {
            writer.write_raw_owned(std::mem::take(&mut frame.frame_bytes))?;
            continue;
        }
        let id = frame.id;
        let reframed = (|| -> Result<(Vec<u8>, bool)> {
            payload.clear();
            decompress_blob_data_into(frame.blob_bytes(), &mut payload)?;
            let (index, tags) = crate::blob_meta::scan_block_ids_and_tags(&payload)?;
            let Some(index) = index else {
                return Err(Error::invalid_data(
                    "the blob's block cannot be described by one index (no elements, a group \
                     carrying more than one element field, or element kinds mixed across \
                     groups), so it cannot be reindexed without re-encoding it",
                ));
            };
            let changed = frame.index != Some(index);
            let tagdata = tags
                .as_ref()
                .and_then(crate::blob_meta::TagIndex::serialize);
            let out = reframe_preserving(
                frame.header_bytes(),
                frame.blob_bytes(),
                &index.serialize(),
                tagdata.as_deref(),
                &mut header_buf,
            )?;
            Ok((out, changed))
        })()
        .map_err(|e| frames.locate(id, e))?;
        let (bytes, changed) = reframed;
        writer.write_raw_owned(bytes)?;
        stats.blobs += 1;
        stats.blobs_changed += u64::from(changed);
    }
    writer.flush()?;
    Ok(stats)
}

/// The frame `[4-byte length][BlobHeader][Blob]` of a blob whose header
/// `header` keeps every field but `indexdata` and `tagdata`, which become
/// `index` and `tagdata`, over the unchanged `blob` bytes. The way-members
/// field and any field this build does not know survive, so a reindexed
/// blob loses nothing its payload still supports. Over the header-size limit
/// the tag index is dropped first, as the writer drops it; still over, the
/// blob is refused rather than stripped of a preserved field. `header_buf`
/// is scratch.
pub(crate) fn reframe_preserving(
    header: &[u8],
    blob: &[u8],
    index: &[u8],
    tagdata: Option<&[u8]>,
    header_buf: &mut Vec<u8>,
) -> Result<Vec<u8>> {
    let mut rebuilt = rebuild_header(header, index, tagdata, header_buf)?;
    if !rebuilt {
        // A blob without tagdata claims no key set.
        rebuilt = rebuild_header(header, index, None, header_buf)?;
    }
    if !rebuilt {
        return Err(crate::error::new_blob_too_large(
            crate::OversizePart::Header,
            header_buf.len() as u64,
            crate::blob::MAX_BLOB_HEADER_SIZE,
        ));
    }
    let header_len = u32::try_from(header_buf.len())
        .map_err(|_| Error::internal("a rebuilt BlobHeader fits the size limit"))?;
    let mut out = Vec::with_capacity(4 + header_buf.len() + blob.len());
    out.extend_from_slice(&header_len.to_be_bytes());
    out.extend_from_slice(header_buf);
    out.extend_from_slice(blob);
    Ok(out)
}

/// `header` with every `indexdata` and `tagdata` field replaced by `index`
/// and `tagdata`, into `out`. `false` when the result reaches the header-size
/// limit.
fn rebuild_header(
    header: &[u8],
    index: &[u8],
    tagdata: Option<&[u8]>,
    out: &mut Vec<u8>,
) -> Result<bool> {
    out.clear();
    crate::writer::strip_blob_header_fields(
        header,
        &[BlobHeaderField::Indexdata, BlobHeaderField::Tagdata],
        out,
    )?;
    protohoggr::encode_bytes_field(out, crate::read::wire_schema::blob_header::INDEXDATA, index);
    if let Some(tagdata) = tagdata {
        protohoggr::encode_bytes_field(
            out,
            crate::read::wire_schema::blob_header::TAGDATA,
            tagdata,
        );
    }
    Ok((out.len() as u64) < crate::blob::MAX_BLOB_HEADER_SIZE)
}
