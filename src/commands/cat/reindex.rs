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
use crate::read::frame::{Frames, RawBlobFrame};
use crate::read::input::InputSource;
use crate::read::wire_schema::BlobHeaderField;
use crate::write::WriteOptions;

/// What a reindex run did. The two change counts are independent (a blob
/// can be in both) and describe what the rewrite changed, not a verdict on
/// the input: `check --index` is the verdict.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReindexStats {
    /// OSMData blobs written, each with indexdata scanned from its payload.
    pub blobs: u64,
    /// Of those, the blobs whose input indexdata was absent, unreadable or
    /// stated a different index. Compared as index values, so a readable
    /// older index version describing the same block is not counted.
    pub indexdata_changed: u64,
    /// Of those, the blobs whose tagdata as written differs from the input's:
    /// gained where the input had none, removed (the header-size fallback
    /// drops it), replaced by a different key set, or freed of repeated
    /// occurrences. The encoding is canonical, so equal bytes are equal sets.
    pub tagdata_changed: u64,
}

impl ReindexStats {
    /// The one-line summary.
    pub fn write_summary(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        writeln!(
            w,
            "Reindexed {} blobs; indexdata changed in {}, tagdata changed in {}",
            self.blobs, self.indexdata_changed, self.tagdata_changed
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
    let host = &host.enter()?;
    let output = crate::output::bind_unissued(output, &host.invocation)?;
    host.invocation.check()?;
    let stats = reindex_to(input.into(), &output, write, host)?;
    output.publish()?;
    Ok(stats)
}

fn reindex_to(
    source: InputSource<'_>,
    output: &crate::output::BoundOutput,
    write: &WriteOptions,
    host: &crate::host::Entered<'_>,
) -> Result<ReindexStats> {
    let path = source.path();
    let mut frames = Frames::of_file_input(
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

    // The walk stays one sequential read on this thread; every later frame
    // is a frame-pool task (a data blob's decompression, scan and reframe on
    // a worker, any other frame passed through as it is), and the frames are
    // written here in input order.
    let mut stats = ReindexStats::default();
    crate::read::frame_pool::map_frames_ordered(
        &mut frames,
        host,
        Some,
        || ReindexScratch {
            payload: Vec::new(),
            header_buf: Vec::new(),
        },
        |scratch: &mut ReindexScratch, mut frame: RawBlobFrame| {
            if !matches!(frame.blob_type, BlobKind::OsmData) {
                return Ok(Reframed {
                    bytes: std::mem::take(&mut frame.frame_bytes),
                    data: None,
                });
            }
            let (bytes, changed) = reindex_frame(&frame, scratch)?;
            Ok(Reframed {
                bytes,
                data: Some(changed),
            })
        },
        |reframed: Reframed| {
            writer.write_raw_owned(reframed.bytes)?;
            if let Some(changed) = reframed.data {
                stats.blobs += 1;
                stats.indexdata_changed += u64::from(changed.indexdata);
                stats.tagdata_changed += u64::from(changed.tagdata);
            }
            Ok(())
        },
    )?;
    writer.flush()?;
    Ok(stats)
}

/// One frame-pool worker's scratch.
struct ReindexScratch {
    payload: Vec<u8>,
    header_buf: Vec<u8>,
}

/// One frame ready to write: its bytes, and for a data blob what changed.
struct Reframed {
    bytes: Vec<u8>,
    data: Option<Changed>,
}

/// Which of a data blob's two claims the rewrite changed; see
/// [`ReindexStats`] for what each means.
#[derive(Clone, Copy)]
struct Changed {
    indexdata: bool,
    tagdata: bool,
}

/// The reframed bytes of one OSMData frame and what the rewrite changed.
fn reindex_frame(frame: &RawBlobFrame, scratch: &mut ReindexScratch) -> Result<(Vec<u8>, Changed)> {
    scratch.payload.clear();
    decompress_blob_data_into(frame.blob_bytes(), &mut scratch.payload)?;
    let (index, tags) = crate::blob_meta::scan_block_ids_and_tags(&scratch.payload)?;
    let Some(index) = index else {
        return Err(Error::invalid_data(
            "the blob's block cannot be described by one index (no elements, a group \
             carrying more than one element field, or element kinds mixed across \
             groups), so it cannot be reindexed without re-encoding it",
        ));
    };
    let tagdata = tags
        .as_ref()
        .and_then(crate::blob_meta::TagIndex::serialize);
    let (out, tagdata_kept) = reframe_preserving(
        frame.header_bytes(),
        frame.blob_bytes(),
        &index.serialize(),
        tagdata.as_deref(),
        &mut scratch.header_buf,
    )?;
    let written_tagdata = tagdata.as_deref().filter(|_| tagdata_kept);
    // `tagdata` is `None` with `tagdata_present` set when the input repeated
    // the field: the rewrite leaves one occurrence or none, a change either
    // way.
    let repeated_tagdata = frame.tagdata_present && frame.tagdata.is_none();
    let changed = Changed {
        indexdata: frame.index != Some(index),
        tagdata: written_tagdata != frame.tagdata.as_deref() || repeated_tagdata,
    };
    Ok((out, changed))
}

/// The frame `[4-byte length][BlobHeader][Blob]` of a blob whose header
/// `header` keeps every field but `indexdata` and `tagdata`, which become
/// `index` and `tagdata`, over the unchanged `blob` bytes. The way-members
/// field and any field this build does not know survive, so a reindexed
/// blob loses nothing its payload still supports. Over the header-size limit
/// the tag index is dropped first, as the writer drops it; still over, the
/// blob is refused rather than stripped of a preserved field. Returns the
/// frame and whether `tagdata` went into it. `header_buf` is scratch.
pub(crate) fn reframe_preserving(
    header: &[u8],
    blob: &[u8],
    index: &[u8],
    tagdata: Option<&[u8]>,
    header_buf: &mut Vec<u8>,
) -> Result<(Vec<u8>, bool)> {
    let mut rebuilt = rebuild_header(header, index, tagdata, header_buf)?;
    let tagdata_kept = rebuilt && tagdata.is_some();
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
    Ok((out, tagdata_kept))
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
