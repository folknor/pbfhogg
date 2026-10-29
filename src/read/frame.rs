//! The one blob-frame decoder.
//!
//! Every PBF read path starts at the same envelope:
//! `[4-byte BE header_len][BlobHeader protobuf][Blob protobuf]`. [`Frames`]
//! is the one state machine that walks it, generic over where the bytes come
//! from. `BlobReader` (streaming, and its seek-and-sentinel header walk),
//! `HeaderWalker` (`pread` at an offset), the raw-frame walks that pass
//! frames through undecoded ([`Frames::next_raw`]) and the input-front probes
//! all hold a `Frames` over their own source and nothing else of the frame
//! format: the length prefix, the EOF policy, both size guards, the header
//! parse, the leading-header rule, the declared-datasize guards, the
//! truncation errors and the provenance stamped on every failure live here
//! once.
//!
//! The frame format used to be decoded in three places with three copies of
//! that policy, and the copies disagreed about everything a user sees: where
//! an error was located, whether it named the blob, how a truncation was
//! worded. The disagreements were pinned rather than fixed until the decoders
//! became one; `src/read/frame_conformance_tests.rs` now holds every source to
//! one expectation per case.
//!
//! # Sources
//!
//! A source is any [`FrameSource`]: every `Read` is one, and
//! `HeaderWalker`'s `PreadSource` is the other. Skipping a payload unread
//! needs [`FrameSkip`], whose implementations keep the I/O shape each caller
//! was measured with: `BufReader::seek_relative` plus one sentinel byte for
//! a seekable stream (a plain `Seek::seek` discards the buffer, see
//! `reference/performance-history.md` "`seek_raw` BufReader-discard fix"),
//! `FileReader::skip` for the command readers (the same for buffered input,
//! `DirectReader::skip` for O_DIRECT), and an extent check against the size
//! captured at open for the pread walker.
//!
//! # EOF policy
//!
//! A frame boundary with 0-3 bytes remaining is clean EOF: a partial length
//! prefix cannot start a frame, and per
//! [`reference/truncation-handling.md`](../../reference/truncation-handling.md)
//! a clean cut at a frame boundary is tolerated. Anything past the prefix is
//! a committed read: truncation there is a hard error, not EOF.
//!
//! # Provenance
//!
//! Every error out of a frame is located at the frame: the input's path (when
//! the source was opened from one), the frame's 0-based ordinal (while the
//! walk counts from the stream start) and the frame's start offset (when the
//! source has a position). The same (ordinal, frame start) pair is what
//! callers stamp on failures of the work they do with a frame afterwards, so
//! a frame-level error and a decode error on the same blob name the same
//! place. The precise extent a truncation broke is in the message.
//! Cancellation is not a fault of the input and stays unlocated.

use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use crate::blob::{
    BlobKind, BlobReaderSource, BlobSite, ByteOffset, MAX_BLOB_DATASIZE, MAX_BLOB_HEADER_SIZE,
    WireBlobHeader, warn_unrecognised_indexdata,
};
use crate::blob_meta::BlobIndex;
use crate::error::{BlobError, Error, Location, Result, new_blob_error};
use crate::file_reader::FileReader;

/// Where frames come from.
pub(crate) trait FrameSource {
    /// Fill `buf` from the current position, stopping short only where the
    /// input ends. Returns the bytes delivered. Used for the length prefix
    /// alone, so a short count is the clean-EOF signal; every other failure
    /// is an error and is never read as end of input.
    fn fill(&mut self, buf: &mut [u8]) -> std::io::Result<usize>;

    /// Append up to `n` bytes to `out`, stopping short only where the input
    /// ends, and return how many were appended. Used for header and payload
    /// bytes, whose shortfall is a truncation.
    fn append(&mut self, n: usize, out: &mut Vec<u8>) -> std::io::Result<usize>;
}

impl<R: Read> FrameSource for R {
    fn fill(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut filled = 0;
        while filled < buf.len() {
            match self.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(filled)
    }

    /// A bounded `read_to_end` into the caller's buffer, reserved to `n`
    /// first: a caller that sized `out` exactly gets no reallocation. Whether
    /// the spare capacity is zero-filled is the reader's: one that implements
    /// only `read` gets std's default `read_buf`, which zero-fills.
    /// `Take` reports a short input as a short count.
    fn append(&mut self, n: usize, out: &mut Vec<u8>) -> std::io::Result<usize> {
        out.reserve(n);
        self.by_ref().take(n as u64).read_to_end(out)
    }
}

/// A source that can pass over a payload without delivering it.
pub(crate) trait FrameSkip: FrameSource {
    /// Move `n` bytes forward. `Ok(false)` when the input ends first.
    fn skip_bytes(&mut self, n: u64) -> std::io::Result<bool>;
}

/// Seek `n - 1` bytes relative (which keeps a `BufReader`'s buffer when the
/// target lies inside it), then read the last byte, because a seek past the
/// end succeeds and only the read can tell. A zero-byte skip reads nothing.
impl<R: BlobReaderSource> FrameSkip for R {
    fn skip_bytes(&mut self, n: u64) -> std::io::Result<bool> {
        if n == 0 {
            return Ok(true);
        }
        let back = i64::try_from(n - 1)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "skip too long"))?;
        self.skip_relative(back)?;
        let mut sentinel = [0u8; 1];
        Ok(self.fill(&mut sentinel)? == 1)
    }
}

/// `FileReader::skip`: the seek-and-sentinel above for buffered input,
/// `DirectReader::skip` (which validates its target itself) for O_DIRECT.
impl FrameSkip for FileReader {
    fn skip_bytes(&mut self, n: u64) -> std::io::Result<bool> {
        match self.skip(n) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
            Err(e) => Err(e),
        }
    }
}

/// Which optional `BlobHeader` fields a walk keeps. Each consumer sets its
/// own: a sequential element read skips tagdata and way members (and, on
/// hot paths that never ask for a blob index, indexdata), while header
/// walks and raw passthrough keep tagdata and indexdata. The flags decide
/// copy versus skip only; see [`WireBlobHeader::parse`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct HeaderFields {
    pub(crate) tagdata: bool,
    pub(crate) indexdata: bool,
    pub(crate) waymembers: bool,
}

impl HeaderFields {
    /// Tagdata and indexdata, no way members: header walks and passthrough.
    pub(crate) const INDEXES: HeaderFields = HeaderFields {
        tagdata: true,
        indexdata: true,
        waymembers: false,
    };
}

/// Which frame a header, payload or error belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FrameId {
    /// Offset of the frame's length prefix; `None` when the source has no
    /// position (a bare `Read`).
    pub(crate) start: Option<u64>,
    /// 0-based ordinal of the frame in its stream; `None` once a seek has
    /// broken the count from the stream start.
    pub(crate) ordinal: Option<u64>,
}

/// A frame's header, parsed. The payload has not been touched.
pub(crate) struct FrameHeader {
    pub(crate) id: FrameId,
    /// Length of the `BlobHeader` protobuf, i.e. the frame's prefix value.
    pub(crate) header_len: usize,
    /// Declared payload length, bounded by `MAX_BLOB_DATASIZE`.
    pub(crate) data_size: usize,
    pub(crate) header: WireBlobHeader,
}

impl FrameHeader {
    /// Bytes consumed by prefix + header, i.e. the offset of the payload
    /// within the frame.
    pub(crate) fn payload_offset(&self) -> usize {
        4 + self.header_len
    }
}

/// A header returned and its payload not yet consumed.
#[derive(Clone, Copy, Debug)]
struct Pending {
    id: FrameId,
    data_size: usize,
    /// For a data frame on a walk that refuses a partially indexed input,
    /// whether its header carried readable indexdata; `None` otherwise. Fed
    /// to the guard once the frame is complete.
    readable_index: Option<bool>,
}

/// The partially-indexed rule, owned by the frame walk: what the first data
/// blob said about indexdata, and where the first data blob without it sits.
/// `L` is that location. Fed every data blob in file order; a file is
/// partial once it has shown both kinds, and the refusal names the first
/// unindexed data blob, whether it came before or after the first indexed
/// one.
#[derive(Clone, Debug)]
pub(crate) struct PartialIndexGuard<L> {
    /// Whether the first data blob carried readable indexdata; `None` until
    /// the walk has seen one.
    first_indexed: Option<bool>,
    /// Where the first data blob without readable indexdata sits; `None`
    /// until one has been seen.
    first_unindexed: Option<L>,
}

impl<L> Default for PartialIndexGuard<L> {
    fn default() -> Self {
        Self {
            first_indexed: None,
            first_unindexed: None,
        }
    }
}

impl<L: Copy> PartialIndexGuard<L> {
    /// Record one data blob, `indexed` when it carries readable indexdata.
    /// Returns the location of the first unindexed data blob once the file
    /// has shown both kinds.
    pub(crate) fn observe(&mut self, indexed: bool, at: L) -> Option<L> {
        if !indexed && self.first_unindexed.is_none() {
            self.first_unindexed = Some(at);
        }
        match self.first_indexed {
            None => {
                self.first_indexed = Some(indexed);
                None
            }
            Some(first) if first != indexed => self.first_unindexed,
            Some(_) => None,
        }
    }
}

/// The refusal [`PartialIndexGuard`] raises, unlocated: the caller locates
/// it at the blob the guard returned. Typed `MissingIndexdata` with
/// `partial`, so a front end can point at the remedy (`pbfhogg cat`
/// indexes every blob).
pub(crate) fn partial_index_error() -> Error {
    crate::error::new_partial_indexdata(
        "the file is partially indexed: indexdata on some data blobs and not \
         others, and this is the first data blob without indexdata",
    )
}

/// The frame state machine over a source `S`. See the module doc.
///
/// Protocol: [`Self::next_header`], then at most one of
/// [`Self::read_payload`] or [`Self::skip_payload`] before the next header.
/// Dropping the walk with a payload pending is fine (a probe that stops at a
/// header); asking for another header first is a bug and an error. Any
/// located failure ends the walk: every later call is an error until
/// [`Self::reposition`] and [`Self::clear_failure`], because a source that
/// failed partway through a frame would otherwise have the rest of that frame
/// read as the next one.
#[derive(Clone, Debug)]
pub(crate) struct Frames<S> {
    pub(crate) src: S,
    /// The source's position: a frame boundary between frames, the payload
    /// start while one is pending. `None` when the source has no position or
    /// a failure left it unknown.
    pos: Option<u64>,
    /// Where the stream starts: the construction position, fixed across
    /// seeks. An OSMHeader is leading only here.
    stream_start: u64,
    /// The ordinal of the next frame; `None` once the count is unknowable.
    next_ordinal: Option<u64>,
    path: Option<Arc<Path>>,
    diagnostics: crate::host::Diagnostics,
    cancel: crate::host::Invocation,
    /// The current frame's `BlobHeader` bytes, reused across frames.
    header_buf: Vec<u8>,
    pub(crate) fields: HeaderFields,
    pending: Option<Pending>,
    failed: bool,
    /// Present when this walk refuses a partially indexed input. Armed by
    /// default on a command input ([`Self::of_input`], and
    /// `BlobReader::open_in`); see [`Self::accept_partial_index`].
    partial_index: Option<PartialIndexGuard<FrameId>>,
}

impl<S> Frames<S> {
    /// A walk over `src`, whose position is `start` (`None` for a source
    /// with none). The ordinal counts from 0 when the walk starts at the
    /// beginning of the input or has no position to say otherwise, and is
    /// unknown for a walk that starts mid-input.
    pub(crate) fn new(src: S, start: Option<u64>, fields: HeaderFields) -> Self {
        Frames {
            src,
            pos: start,
            stream_start: start.unwrap_or(0),
            next_ordinal: matches!(start, None | Some(0)).then_some(0),
            path: None,
            diagnostics: crate::host::Diagnostics::discard(),
            cancel: crate::host::Invocation::new(),
            header_buf: Vec::new(),
            fields,
            pending: None,
            failed: false,
            partial_index: None,
        }
    }

    /// A walk over a command input from its first byte, with the run's
    /// diagnostics and cancellation, its errors naming `path`. It refuses a
    /// partially indexed input ([`Self::refuse_partial_index`]); a command
    /// exempt from that policy opts out with [`Self::accept_partial_index`].
    pub(crate) fn of_input(src: S, path: &Path, host: &crate::host::Entered<'_>) -> Self {
        let mut frames = Self::new(src, Some(0), HeaderFields::INDEXES);
        frames.path = Some(Arc::from(path));
        frames.diagnostics = host.diagnostics.clone();
        frames.cancel = host.invocation.clone();
        frames.refuse_partial_index();
        frames
    }

    /// Refuse a partially indexed input: data blobs with readable indexdata
    /// on some and not others, in either order. Once a completed data frame
    /// shows the mixture, the walk ends with the typed missing-indexdata
    /// error (`partial`), located at the first data blob without readable
    /// indexdata. A wholly indexed and a wholly unindexed input both pass;
    /// whether a wholly unindexed one is acceptable stays the caller's
    /// first-blob gate. Indexdata that is present but unreadable counts as
    /// absent, as it does for every consumer.
    ///
    /// This is the project's policy for command inputs (ruled 2026-10-26):
    /// a command fails loudly on such a file and the front end points at
    /// `pbfhogg cat`, which indexes every blob, rather than each path
    /// arguing whether it could cope with the missing indexdata. It is
    /// judged here, in the one frame walk every reader shares, so a command
    /// cannot accept the file on one path and refuse it on another. The
    /// observation needs the indexdata parsed, so an armed walk parses it
    /// even where the caller asked not to keep it, and then drops it.
    pub(crate) fn refuse_partial_index(&mut self) {
        self.partial_index = Some(PartialIndexGuard::default());
    }

    /// Exempt this walk from [`Self::refuse_partial_index`]. Only for a
    /// command whose job is the file as it is: `cat` (the remedy), `check
    /// --index` (which reports the mixture as a violation) and `inspect`'s
    /// descriptions. The call site says which.
    pub(crate) fn accept_partial_index(&mut self) {
        self.partial_index = None;
    }

    pub(crate) fn set_path(&mut self, path: Option<Arc<Path>>) {
        self.path = path;
    }

    pub(crate) fn path(&self) -> Option<&Arc<Path>> {
        self.path.as_ref()
    }

    pub(crate) fn set_diagnostics(&mut self, diagnostics: crate::host::Diagnostics) {
        self.diagnostics = diagnostics;
    }

    pub(crate) fn set_cancel(&mut self, cancel: crate::host::Invocation) {
        self.cancel = cancel;
    }

    pub(crate) fn cancel_token(&self) -> &crate::host::Invocation {
        &self.cancel
    }

    /// The source's current position, when known.
    pub(crate) fn position(&self) -> Option<u64> {
        self.pos
    }

    /// The identity the next frame will have: the current position and
    /// ordinal. Meaningful between frames.
    pub(crate) fn next_id(&self) -> FrameId {
        FrameId {
            start: self.pos,
            ordinal: self.next_ordinal,
        }
    }

    /// Whether a failure has ended the walk.
    pub(crate) fn failed(&self) -> bool {
        self.failed
    }

    /// End the walk after a failure the caller found in a frame this walk
    /// delivered (a `Blob` envelope that does not parse, say).
    pub(crate) fn poison(&mut self) {
        self.failed = true;
        self.pending = None;
        self.pos = None;
    }

    /// The source was moved to `pos` (`None`: a move that failed, leaving the
    /// position unknown). Any pending payload is forgotten. A successful move
    /// to the start of an input that begins at 0 restarts the ordinal count;
    /// any other target loses it, because a stale count stamped on a later
    /// error would name the wrong frame. A failed move loses both the
    /// position and the count: the walk goes on as a positionless one, which
    /// names no offset and no blob and never takes an OSMHeader as leading.
    /// A failure is not cleared: see [`Self::clear_failure`].
    pub(crate) fn reposition(&mut self, pos: Option<u64>) {
        self.pending = None;
        // A refusing walk stays refusing, but what it saw before the move is
        // forgotten: a rewind restarts the traversal, and after any other
        // move the guard judges only the frames it walks from there.
        if let Some(guard) = self.partial_index.as_mut() {
            *guard = PartialIndexGuard::default();
        }
        self.pos = pos;
        self.next_ordinal = pos.and_then(|pos| (pos == 0 && self.stream_start == 0).then_some(0));
    }

    /// Resume a walk that a failure ended, after the caller repositioned it.
    pub(crate) fn clear_failure(&mut self) {
        self.failed = false;
    }

    /// The location of frame `id` in this walk's input.
    #[cold]
    pub(crate) fn location(&self, id: FrameId) -> Location {
        Location::new(self.path.as_ref(), id.ordinal, id.start.map(ByteOffset))
    }

    /// Locate a failure of the work done with frame `id` after it was read
    /// (its decode, say) at that frame. A cancellation stays unlocated and a
    /// location already attached is kept.
    #[cold]
    pub(crate) fn locate(&self, id: FrameId, err: Error) -> Error {
        err.locate_blob(self.location(id))
    }

    /// Locate `err` at frame `id` and end the walk.
    #[cold]
    fn fail(&mut self, id: FrameId, err: Error) -> Error {
        let err = err.locate(self.location(id));
        self.poison();
        err
    }

    /// The pending frame, or the error for asking without one (which, as a
    /// misuse of the walk, ends it like any other failure).
    fn take_pending(&mut self) -> Result<Pending> {
        if self.failed {
            return Err(used_after_failure());
        }
        match self.pending.take() {
            Some(pending) => Ok(pending),
            None => {
                self.poison();
                Err(Error::internal(
                    "frame payload requested with no frame header read",
                ))
            }
        }
    }

    /// The pending payload was consumed in full: the walk is at the next
    /// frame boundary.
    ///
    /// A refusing walk then judges the completed frame: it is observed only
    /// once whole, so a frame whose payload is truncated is the truncation
    /// error, and a probe that drops the walk at a header observes nothing.
    fn finish(&mut self, pending: Pending) -> Result<()> {
        self.pos = self
            .pos
            .map(|payload_start| payload_start + pending.data_size as u64);
        self.next_ordinal = self.next_ordinal.map(|n| n + 1);
        if let Some(readable) = pending.readable_index
            && let Some(guard) = self.partial_index.as_mut()
            && let Some(first_unindexed) = guard.observe(readable, pending.id)
        {
            // Located at the first unindexed data blob, which may be an
            // earlier frame than this one, and then the walk ends.
            let err = partial_index_error().locate(self.location(first_unindexed));
            self.poison();
            return Err(err);
        }
        Ok(())
    }
}

impl<S: FrameSource> Frames<S> {
    /// Read the next frame's length prefix and `BlobHeader`, leaving the
    /// source at the payload. `Ok(None)` at clean EOF.
    ///
    /// A tripped cancellation token ends the walk with `Cancelled`,
    /// unlocated; it is checked here only, before a frame is started, so a
    /// frame whose header was read is always finished.
    pub(crate) fn next_header(&mut self) -> Result<Option<FrameHeader>> {
        if self.failed {
            return Err(used_after_failure());
        }
        if self.pending.is_some() {
            self.poison();
            return Err(Error::internal(
                "next frame requested before the last frame's payload was read or skipped",
            ));
        }
        if let Err(e) = self.cancel.check() {
            self.poison();
            return Err(e);
        }
        let id = FrameId {
            start: self.pos,
            ordinal: self.next_ordinal,
        };
        match self.read_header(id) {
            Ok(header) => Ok(header),
            Err(e) => Err(self.fail(id, e)),
        }
    }

    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    fn read_header(&mut self, id: FrameId) -> Result<Option<FrameHeader>> {
        let mut prefix = [0u8; 4];
        let got = self.src.fill(&mut prefix)?;
        if got < prefix.len() {
            // Clean EOF. A 1-3 byte tail was consumed: keep the position
            // with the source, so a stream that delivers more later is not
            // read as starting at this boundary.
            self.pos = id.start.map(|s| s + got as u64);
            return Ok(None);
        }
        let header_len = check_header_len(u32::from_be_bytes(prefix))?;

        self.header_buf.clear();
        let header_start = id.start.map(|s| s + 4);
        let got = match self.src.append(header_len, &mut self.header_buf) {
            Ok(got) => got,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => 0,
            Err(e) => return Err(e.into()),
        };
        if got < header_len {
            return Err(truncated(TruncatedPart::Header, header_len, header_start));
        }

        let refusing = self.partial_index.is_some();
        let mut header = WireBlobHeader::parse(
            &self.header_buf,
            self.fields.tagdata,
            self.fields.indexdata || refusing,
            self.fields.waymembers,
        )?;
        // The partial-index observation: readable means a claim a consumer
        // could act on (an unknown version or kind, an inverted range or a
        // repeated field read as absent), not merely a present field.
        let readable_index =
            (refusing && matches!(header.blob_type, BlobKind::OsmData)).then(|| {
                header
                    .indexdata
                    .as_ref()
                    .and_then(|data| BlobIndex::deserialize(data))
                    .is_some()
            });
        if refusing && !self.fields.indexdata {
            // Parsed for the guard only: the caller sees what it asked for,
            // and no warning about a field it did not ask to read.
            header.indexdata = None;
            header.indexdata_present = false;
            header.unrecognised_indexdata_len = None;
        }
        if let Some(len) = header.unrecognised_indexdata_len {
            warn_unrecognised_indexdata(
                &self.diagnostics,
                &self.cancel,
                len,
                BlobSite {
                    source: self.path.as_deref(),
                    offset: id.start,
                },
            );
        }
        // A positioned walk knows its stream start; one without a position
        // knows only whether this is the first frame it counted.
        let at_start = match id.start {
            Some(start) => start == self.stream_start,
            None => id.ordinal == Some(0),
        };
        check_header_position(&header.blob_type, at_start)?;
        if header.datasize < 0 {
            return Err(new_blob_error(BlobError::InvalidDataSize {
                size: header.datasize,
            }));
        }
        // Reject an oversized declared datasize before any caller allocates,
        // reads or skips the payload. `MAX_BLOB_MESSAGE_SIZE` caps the
        // decompressed block only after the compressed body is in memory, so
        // without this a hostile datasize drives an outsized allocation.
        let datasize = header.datasize as u64;
        if datasize >= MAX_BLOB_DATASIZE {
            return Err(new_blob_error(BlobError::DataSizeTooBig { size: datasize }));
        }
        let data_size = datasize as usize;

        self.pos = header_start.map(|s| s + header_len as u64);
        self.pending = Some(Pending {
            id,
            data_size,
            readable_index,
        });
        Ok(Some(FrameHeader {
            id,
            header_len,
            data_size,
            header,
        }))
    }

    /// Append the pending payload to `out`.
    pub(crate) fn read_payload(&mut self, out: &mut Vec<u8>) -> Result<()> {
        let pending = self.take_pending()?;
        let got = match self.src.append(pending.data_size, out) {
            Ok(got) => got,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => 0,
            Err(e) => return Err(self.fail(pending.id, e.into())),
        };
        if got < pending.data_size {
            let err = truncated(TruncatedPart::Payload, pending.data_size, self.pos);
            return Err(self.fail(pending.id, err));
        }
        self.finish(pending)
    }

    /// The next frame whole, as raw bytes for passthrough or selective
    /// decode: the prefix, the exact header bytes and the payload in one
    /// buffer sized to the frame.
    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    #[allow(clippy::cast_possible_truncation)]
    pub(crate) fn next_raw(&mut self) -> Result<Option<RawBlobFrame>> {
        let Some(header) = self.next_header()? else {
            return Ok(None);
        };
        let blob_offset = header.payload_offset();
        let mut frame_bytes = Vec::with_capacity(blob_offset + header.data_size);
        frame_bytes.extend_from_slice(&(header.header_len as u32).to_be_bytes());
        frame_bytes.extend_from_slice(&self.header_buf);
        self.read_payload(&mut frame_bytes)?;
        let WireBlobHeader {
            blob_type,
            indexdata,
            indexdata_present,
            tagdata,
            tagdata_present,
            ..
        } = header.header;
        Ok(Some(RawBlobFrame {
            frame_bytes,
            blob_type,
            blob_offset,
            index: indexdata.and_then(|data| BlobIndex::deserialize(&data)),
            index_present: indexdata_present,
            tagdata,
            tagdata_present,
            id: header.id,
        }))
    }
}

impl<S: FrameSkip> Frames<S> {
    /// Pass over the pending payload without reading it.
    pub(crate) fn skip_payload(&mut self) -> Result<()> {
        let pending = self.take_pending()?;
        match self.src.skip_bytes(pending.data_size as u64) {
            Ok(true) => self.finish(pending),
            Ok(false) => {
                let err = truncated(TruncatedPart::Payload, pending.data_size, self.pos);
                Err(self.fail(pending.id, err))
            }
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                let err = truncated(TruncatedPart::Payload, pending.data_size, self.pos);
                Err(self.fail(pending.id, err))
            }
            Err(e) => Err(self.fail(pending.id, e.into())),
        }
    }
}

impl<S: Read + std::io::Seek> Frames<S> {
    /// Seek the source to `pos` and reposition the walk there. A failure
    /// leaves the position unknown and is located at the target when the
    /// target is absolute.
    pub(crate) fn seek(&mut self, pos: std::io::SeekFrom) -> Result<u64> {
        match std::io::Seek::seek(&mut self.src, pos) {
            Ok(offset) => {
                self.reposition(Some(offset));
                Ok(offset)
            }
            Err(e) => {
                self.reposition(None);
                let target = match pos {
                    std::io::SeekFrom::Start(offset) => Some(ByteOffset(offset)),
                    std::io::SeekFrom::End(_) | std::io::SeekFrom::Current(_) => None,
                };
                Err(Error::from(e).locate(Location::new(self.path.as_ref(), None, target)))
            }
        }
    }
}

#[cold]
fn used_after_failure() -> Error {
    Error::internal("frame walk used after a failed frame")
}

/// Which part of a frame a truncation cut.
#[derive(Clone, Copy)]
enum TruncatedPart {
    Header,
    Payload,
}

/// The one truncation error: the frame's declared part of `len` bytes from
/// `start` runs past the end of the input. Worded from what was declared,
/// not from how the shortfall was noticed, so every source says the same
/// thing; the extent is omitted when the source has no position.
#[cold]
fn truncated(part: TruncatedPart, len: usize, start: Option<u64>) -> Error {
    let (what, by) = match part {
        TruncatedPart::Header => ("BlobHeader", "its length prefix"),
        TruncatedPart::Payload => ("Blob", "its BlobHeader"),
    };
    let extent = match start {
        Some(start) => format!(" at bytes [{start}, {})", start + len as u64),
        None => String::new(),
    };
    Error::io_kind(
        std::io::ErrorKind::UnexpectedEof,
        format!(
            "frame truncated: {by} declares a {len}-byte {what}{extent}, past the end of the input"
        ),
    )
}

/// Apply both length-prefix guards and return the validated header length.
///
/// * A zero-length `BlobHeader` consumes 4 bytes and declares nothing: no
///   type, `datasize == 0`. Accepting it turns a run of zero bytes into a
///   stream of legal empty frames that every consumer skips as "unknown blob
///   type", so infinite garbage reads as a clean success with empty output.
/// * A length at or above [`MAX_BLOB_HEADER_SIZE`] is beyond the format's cap,
///   and honouring it means allocating up to 4 GiB on the strength of four
///   unvalidated bytes.
#[allow(clippy::cast_possible_truncation)]
fn check_header_len(header_len: u32) -> Result<usize> {
    let len = u64::from(header_len);
    if len == 0 {
        return Err(new_blob_error(BlobError::InvalidHeaderSize));
    }
    if len >= MAX_BLOB_HEADER_SIZE {
        return Err(new_blob_error(BlobError::HeaderTooBig { size: len }));
    }
    Ok(header_len as usize)
}

/// OSMHeader is allowed only at the start of a PBF stream. Ignoring a later
/// header discards its feature requirements and metadata while interpreting
/// its data under the first header. libosmium likewise expects OSMData after
/// the leading OSMHeader (pbf_input_format.hpp). Unknown extension blobs keep
/// their existing skip policy; low-level readers may still read data alone.
/// For a seekable BlobReader, the stream starts at its construction position,
/// which can be nonzero for an embedded PBF; later seeks keep that origin.
fn check_header_position(blob_type: &BlobKind, at_start: bool) -> Result<()> {
    if matches!(blob_type, BlobKind::OsmHeader) && !at_start {
        return Err(crate::error::new_wire_error(
            "a second OSMHeader blob follows the first frame; OSMHeader is allowed only as the first blob",
        ));
    }
    Ok(())
}

/// A raw blob frame for passthrough or selective decode.
///
/// The Blob protobuf bytes are a suffix of `frame_bytes` starting at
/// `blob_offset`, eliminating a separate allocation per blob.
pub(crate) struct RawBlobFrame {
    /// Complete framed bytes: `[4-byte header_len][BlobHeader][Blob]`.
    pub(crate) frame_bytes: Vec<u8>,
    pub(crate) blob_type: BlobKind,
    /// Byte offset within `frame_bytes` where the Blob protobuf starts.
    pub(crate) blob_offset: usize,
    /// Blob-level index from BlobHeader indexdata, if present *and* readable.
    pub(crate) index: Option<BlobIndex>,
    /// Whether the header carried a non-empty indexdata field at all.
    ///
    /// `index` collapses "absent" and "present but unreadable" into `None`,
    /// which is correct for every reader - an index it cannot read is an index
    /// it must not act on - and wrong for `check --index`, whose entire job is
    /// to notice that the file claims something it cannot back up. Malformed
    /// indexdata was passing verification silently until this field existed.
    #[cfg_attr(not(feature = "commands"), allow(dead_code))]
    pub(crate) index_present: bool,
    /// Per-blob tag key data from BlobHeader field 4, if present. Retained
    /// so `check --index` can compare the stored claim with decoded tags.
    #[cfg_attr(not(feature = "commands"), allow(dead_code))]
    pub(crate) tagdata: Option<Box<[u8]>>,
    /// Non-empty tagdata was present, including duplicate unreadable claims.
    #[cfg_attr(not(feature = "commands"), allow(dead_code))]
    pub(crate) tagdata_present: bool,
    /// Which frame this is: its start offset and ordinal. Failures of the
    /// work done with the frame are located through [`Frames::locate`].
    pub(crate) id: FrameId,
}

impl RawBlobFrame {
    /// The raw Blob protobuf message bytes (for selective decoding).
    pub(crate) fn blob_bytes(&self) -> &[u8] {
        &self.frame_bytes[self.blob_offset..]
    }

    /// The raw `BlobHeader` protobuf message bytes, sitting between the
    /// 4-byte big-endian length prefix and the Blob payload. Callers that
    /// reframe a passthrough blob use this to preserve every original header
    /// field verbatim rather than reconstructing fields from parsed values.
    pub(crate) fn header_bytes(&self) -> &[u8] {
        &self.frame_bytes[4..self.blob_offset]
    }

    /// Byte offset of this frame in its input (a raw-frame walk always
    /// starts at offset 0 and so always knows it).
    #[cfg_attr(not(feature = "commands"), allow(dead_code))]
    pub(crate) fn file_offset(&self) -> u64 {
        self.id.start.unwrap_or(0)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{FrameId, Frames, HeaderFields, check_header_len};
    use crate::error::{BlobError, ErrorKind};

    fn frames(bytes: Vec<u8>) -> Frames<std::io::Cursor<Vec<u8>>> {
        Frames::new(std::io::Cursor::new(bytes), Some(0), HeaderFields::INDEXES)
    }

    #[test]
    fn only_leading_header_frames_are_accepted() {
        use crate::blob::BlobKind;

        super::check_header_position(&BlobKind::OsmHeader, true).unwrap();
        let err = super::check_header_position(&BlobKind::OsmHeader, false).unwrap_err();
        assert!(err.to_string().contains("a second OSMHeader blob"));
        for at_start in [false, true] {
            super::check_header_position(&BlobKind::OsmData, at_start).unwrap();
            super::check_header_position(&BlobKind::Unknown("extension".into()), at_start).unwrap();
        }
    }

    /// The two prefix guards at their exact boundaries. The cap is strict
    /// (`>=`) so the largest accepted prefix is one below it.
    #[test]
    fn header_len_guards_bound_the_prefix() {
        assert!(matches!(
            check_header_len(0).unwrap_err().kind(),
            ErrorKind::Blob(BlobError::InvalidHeaderSize)
        ));
        assert_eq!(check_header_len(1).unwrap(), 1);

        #[allow(clippy::cast_possible_truncation)]
        let cap = crate::blob::MAX_BLOB_HEADER_SIZE as u32;
        assert_eq!(check_header_len(cap - 1).unwrap(), cap as usize - 1);
        match check_header_len(cap).unwrap_err().kind() {
            ErrorKind::Blob(BlobError::HeaderTooBig { size }) => {
                assert_eq!(*size, u64::from(cap));
            }
            other => panic!("expected HeaderTooBig, got {other:?}"),
        }
        // The worst case the guard exists for: four 0xFF bytes ask for a 4 GiB
        // allocation. It must be refused by the size guard, not by the
        // allocator.
        match check_header_len(u32::MAX).unwrap_err().kind() {
            ErrorKind::Blob(BlobError::HeaderTooBig { size }) => {
                assert_eq!(*size, u64::from(u32::MAX));
            }
            other => panic!("expected HeaderTooBig, got {other:?}"),
        }
    }

    /// 0-3 trailing bytes at a frame boundary are clean EOF; a complete
    /// prefix is a committed frame. Every source shares this through
    /// `Frames`; the conformance table runs it through each of them.
    #[test]
    fn a_short_tail_is_clean_eof_and_a_complete_prefix_is_not() {
        for tail in 0..4usize {
            assert!(
                frames(vec![0xFFu8; tail]).next_header().unwrap().is_none(),
                "a {tail}-byte tail must read as clean EOF"
            );
        }
        // A 4-byte input declaring 0xFFFFFFFF is the unbounded-allocation
        // case and must fail rather than be excused as EOF.
        assert!(frames(vec![0xFFu8; 4]).next_header().is_err());

        // The tail a stream delivered is consumed, and the position keeps up
        // with it, so a stream that delivers more later is not read as
        // starting back at the boundary.
        let mut walk = frames(vec![0xAA; 2]);
        assert!(walk.next_header().unwrap().is_none());
        assert_eq!(walk.position(), Some(2));
    }

    /// A prefix promising header bytes the input does not have is a
    /// committed read: a hard error located at the frame, never EOF.
    #[test]
    fn truncated_header_bytes_are_an_error_not_eof() {
        // Declares 16 header bytes, supplies 3.
        let mut input = vec![0u8, 0, 0, 16];
        input.extend_from_slice(&[0x0A, 0x07, b'O']);
        let err = frames(input).next_header().err().unwrap();
        assert!(
            matches!(err.kind(), ErrorKind::Io(e) if e.kind() == std::io::ErrorKind::UnexpectedEof),
            "{err:?}"
        );
        assert_eq!(err.byte_offset(), Some(crate::ByteOffset(0)));
        assert_eq!(err.blob_index(), Some(0));
        assert!(
            err.to_string()
                .contains("declares a 16-byte BlobHeader at bytes [4, 20)"),
            "{err}"
        );
    }

    /// One `OSMData` frame with a `len`-byte raw payload.
    fn data_frame(len: u8) -> Vec<u8> {
        let header = [0x0A, 7, b'O', b'S', b'M', b'D', b'a', b't', b'a', 0x18, len];
        let mut out = u32::try_from(header.len()).unwrap().to_be_bytes().to_vec();
        out.extend_from_slice(&header);
        out.extend(std::iter::repeat_n(0u8, usize::from(len)));
        out
    }

    /// The walk protocol: a header, then its payload, then the next header;
    /// asking out of turn is an error, and a failure ends the walk until it
    /// is repositioned and resumed.
    #[test]
    fn the_walk_enforces_header_then_payload_and_stops_after_a_failure() {
        let mut bytes = data_frame(3);
        bytes.extend_from_slice(&data_frame(5));
        let mut walk = frames(bytes.clone());

        assert!(walk.read_payload(&mut Vec::new()).is_err(), "no header yet");
        let mut walk = frames(bytes.clone());
        let first = walk.next_header().unwrap().unwrap();
        assert_eq!(
            first.id,
            FrameId {
                start: Some(0),
                ordinal: Some(0)
            }
        );
        assert!(walk.next_header().is_err(), "payload still pending");
        assert!(walk.failed(), "a misuse ends the walk like any failure");
        assert!(walk.skip_payload().is_err());

        let mut walk = frames(bytes.clone());
        walk.next_header().unwrap().unwrap();
        walk.skip_payload().unwrap();
        let second = walk.next_header().unwrap().unwrap();
        assert_eq!(
            second.id,
            FrameId {
                start: Some(18),
                ordinal: Some(1)
            }
        );
        let mut payload = Vec::new();
        walk.read_payload(&mut payload).unwrap();
        assert_eq!(payload.len(), 5);
        assert!(walk.next_header().unwrap().is_none());

        // A payload cut short fails at its frame and poisons the walk.
        let cut = bytes[..bytes.len() - 1].to_vec();
        let mut walk = frames(cut);
        walk.next_header().unwrap().unwrap();
        walk.read_payload(&mut Vec::new()).unwrap();
        walk.next_header().unwrap().unwrap();
        let err = walk.read_payload(&mut Vec::new()).unwrap_err();
        assert_eq!(err.byte_offset(), Some(crate::ByteOffset(18)));
        assert_eq!(err.blob_index(), Some(1));
        assert!(walk.failed());
        assert!(walk.next_header().is_err(), "a failed walk stays failed");
        walk.seek(std::io::SeekFrom::Start(0)).unwrap();
        walk.clear_failure();
        assert_eq!(walk.next_header().unwrap().unwrap().id.ordinal, Some(0));
    }

    /// What a fixture data blob's header carries.
    #[derive(Clone, Copy)]
    enum Index {
        /// Indexdata scanned from the payload.
        Readable,
        /// No indexdata.
        Absent,
        /// Indexdata present on the wire but unreadable (an inverted range).
        Unreadable,
    }

    /// A header, then one single-node data blob per entry.
    fn pbf(blobs: &[Index]) -> Vec<u8> {
        let mut writer =
            crate::writer::PbfWriter::new(Vec::new(), crate::writer::Compression::None);
        writer
            .write_header(&crate::block_builder::HeaderBuilder::new().build().unwrap())
            .unwrap();
        let mut block = crate::block_builder::BlockBuilder::new();
        for (id, index) in (1_i64..).zip(blobs) {
            block.add_node(id, 0, 0, std::iter::empty::<(&str, &str)>(), None);
            let bytes = block.take().unwrap().unwrap();
            match index {
                Index::Readable => writer.write_primitive_block(bytes).unwrap(),
                Index::Absent => writer.write_primitive_block_no_indexdata(bytes).unwrap(),
                Index::Unreadable => writer
                    .write_primitive_block_owned(crate::block_builder::OwnedBlock {
                        bytes: bytes.to_vec(),
                        index: crate::blob_meta::BlobIndex {
                            kind: crate::ElementKind::Node,
                            min_id: id + 1,
                            max_id: id,
                            count: 1,
                            bbox: None,
                        },
                        tagdata: None,
                        way_members: None,
                    })
                    .unwrap(),
            }
        }
        writer.into_inner().unwrap()
    }

    /// Walk every frame, reading or skipping each payload; the frames
    /// completed, or the error.
    fn walk_all(walk: &mut Frames<std::io::Cursor<Vec<u8>>>, read: bool) -> crate::Result<usize> {
        let mut n = 0;
        while walk.next_header()?.is_some() {
            if read {
                walk.read_payload(&mut Vec::new())?;
            } else {
                walk.skip_payload()?;
            }
            n += 1;
        }
        Ok(n)
    }

    fn refusing(bytes: Vec<u8>) -> Frames<std::io::Cursor<Vec<u8>>> {
        let mut walk = frames(bytes);
        walk.refuse_partial_index();
        walk
    }

    /// A refusing walk stops a mixture at its first data blob without
    /// readable indexdata, in either order, reading or skipping payloads;
    /// indexdata present but unreadable counts as absent; a uniform file
    /// passes; an accepting walk never refuses.
    #[test]
    fn a_refusing_walk_refuses_a_mixture_at_the_first_unreadable_index() {
        use Index::{Absent as A, Readable as R, Unreadable as U};
        // (pattern, ordinal of the first data blob without readable
        // indexdata; the header is 0).
        for (pattern, first) in [
            (&[R, R, A, R][..], 3_u64),
            (&[A, R][..], 1),
            (&[A, A, R][..], 1),
            (&[R, U][..], 2),
            (&[U, R][..], 1),
        ] {
            for read in [false, true] {
                let err = walk_all(&mut refusing(pbf(pattern)), read)
                    .expect_err("a partially indexed walk ran");
                assert!(
                    matches!(
                        err.kind(),
                        ErrorKind::MissingIndexdata { partial: true, .. }
                    ),
                    "{err:?}"
                );
                assert_eq!(err.blob_index(), Some(first));
                let mut accepting = refusing(pbf(pattern));
                accepting.accept_partial_index();
                assert_eq!(walk_all(&mut accepting, read).unwrap(), pattern.len() + 1);
            }
        }
        for pattern in [&[R, R][..], &[A, A][..], &[U, A][..], &[][..]] {
            assert_eq!(
                walk_all(&mut refusing(pbf(pattern)), false).unwrap(),
                pattern.len() + 1
            );
        }
    }

    /// The frame that shows the mixture is judged only once complete: a
    /// payload cut short there is the truncation error, not the refusal.
    #[test]
    fn a_truncated_detecting_frame_is_the_truncation() {
        let bytes = pbf(&[Index::Readable, Index::Absent]);
        let cut = bytes[..bytes.len() - 1].to_vec();
        let err = walk_all(&mut refusing(cut), true).unwrap_err();
        assert!(
            matches!(err.kind(), ErrorKind::Io(io) if io.kind() == std::io::ErrorKind::UnexpectedEof),
            "{err:?}"
        );
    }

    /// An armed walk parses indexdata to observe it, but a caller that did
    /// not ask to keep it still gets none.
    #[test]
    fn an_armed_walk_keeps_only_the_fields_asked_for() {
        let mut walk = Frames::new(
            std::io::Cursor::new(pbf(&[Index::Readable])),
            Some(0),
            HeaderFields {
                tagdata: false,
                indexdata: false,
                waymembers: false,
            },
        );
        walk.refuse_partial_index();
        walk.next_header().unwrap().unwrap();
        walk.skip_payload().unwrap();
        let data = walk.next_header().unwrap().unwrap();
        assert!(data.header.indexdata.is_none());
        assert!(!data.header.indexdata_present);
    }

    /// A move forgets what the guard saw before it, and the walk stays
    /// refusing: an indexed blob seen before a seek and an unindexed one
    /// after are not judged together, while the same file walked through is
    /// refused.
    #[test]
    fn a_reposition_resets_what_the_guard_saw() {
        let bytes = pbf(&[Index::Readable, Index::Absent, Index::Readable]);
        let mut walk = refusing(bytes.clone());
        for _ in 0..2 {
            walk.next_header().unwrap().unwrap();
            walk.skip_payload().unwrap();
        }
        let second = walk.next_header().unwrap().unwrap().id.start.unwrap();
        walk.seek(std::io::SeekFrom::Start(second)).unwrap();
        walk.next_header().unwrap().unwrap();
        walk.skip_payload().unwrap();
        let err = walk
            .next_header()
            .and_then(|_| walk.skip_payload())
            .unwrap_err();
        assert!(
            matches!(
                err.kind(),
                ErrorKind::MissingIndexdata { partial: true, .. }
            ),
            "still refusing after the move: {err:?}"
        );
        assert!(walk_all(&mut refusing(bytes), false).is_err());
    }
}
