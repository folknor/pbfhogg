//! Pread-only blob header walker.
//!
//! Walks a PBF file's blob headers by `pread`-ing just the length prefix
//! and header bytes per blob, skipping data bytes without reading them.
//! The file is opened with `posix_fadvise(RANDOM)` so the kernel does
//! not speculatively prefetch the blob bodies that the header walk
//! skips over.
//!
//! # When to use
//!
//! - Command paths that walk headers to build a per-blob plan and then
//!   selectively read a subset of blob bodies (e.g. `getid` include mode,
//!   `diff` parallel shard planner).
//! - Callers that want to pread each blob's body on a worker thread via
//!   the shared `Arc<File>` rather than a single-threaded sequential
//!   reader.
//!
//! # When not to use
//!
//! - Sequential streaming reads that need every blob's body in order.
//!   The `BlobReader` + buffered `FileReader` path is the right choice
//!   there; its 256 KB buffer and `fadvise(SEQUENTIAL)` let the kernel
//!   keep a pipeline of blob bodies in flight.

use std::path::Path;
use std::sync::Arc;

use crate::blob::BlobKind;
use crate::blob_meta::BlobIndex;
use crate::error::Result;
use crate::frame::Frames;
use crate::tunables::{HEADER_PROBE_SIZE, SAMPLE_CAP};

/// Estimate of the OSMData blob count of a PBF, from a bounded probe walk of
/// leading blob headers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BlobCountEstimate {
    /// OSMData blobs. Exact when [`Self::exact`] is true.
    pub(crate) osmdata_blobs: u64,
    /// The probe walk reached EOF before its sample cap.
    pub(crate) exact: bool,
}

/// Arm selected for a command that can either walk blob headers (reading
/// bodies selectively via pread) or scan the whole file in one pass.
///
/// The full-scan mechanism is per-command: getparents decodes a large byte
/// fraction and uses the pipelined reader; getid include streams frames
/// sequentially and decodes only prescreen survivors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScanArm {
    Walker,
    FullScan,
}

/// Estimate the number of OSMData frames using a bounded header-only probe.
///
/// The first header frame and unknown frames are intentionally excluded from
/// the frame-size mean: only OSMData frame bytes predict OSMData count.
pub(crate) fn estimate_blob_count<'a>(
    input: impl Into<crate::read::input::InputSource<'a>>,
    host: &crate::host::Host,
) -> Result<BlobCountEstimate> {
    let source = input.into();
    let path = source.path();
    let mut walker = HeaderWalker::open(source, host)?;
    let file_size = walker.file_size();
    if file_size == 0 {
        return Err(crate::error::new_missing_header(path));
    }
    let mut frames = 0_usize;
    let mut osmdata_blobs = 0_u64;
    let mut sampled_osmdata_bytes = 0_u64;
    let mut sampled_end = 0_u64;

    while frames < SAMPLE_CAP {
        let Some(meta) = walker.next_header()? else {
            return Ok(BlobCountEstimate {
                osmdata_blobs,
                exact: true,
            });
        };
        frames += 1;
        if frames == 1 && meta.blob_type != BlobKind::OsmHeader {
            return Err(crate::error::new_missing_header(path));
        }
        if meta.blob_type == BlobKind::OsmData {
            osmdata_blobs += 1;
            sampled_osmdata_bytes += meta.frame_size as u64;
            sampled_end = meta.frame_start + meta.frame_size as u64;
        }
    }

    if osmdata_blobs == 0 {
        // A cap of entirely non-data frames cannot provide a useful mean.
        // Conservatively choose the cheap walker rather than inventing a
        // high-count estimate.
        return Ok(BlobCountEstimate {
            osmdata_blobs: 0,
            exact: false,
        });
    }
    let mean_frame_bytes = sampled_osmdata_bytes / osmdata_blobs;
    if mean_frame_bytes == 0 {
        return Ok(BlobCountEstimate {
            osmdata_blobs,
            exact: false,
        });
    }
    let remaining_bytes = file_size.saturating_sub(sampled_end);
    Ok(BlobCountEstimate {
        osmdata_blobs: osmdata_blobs + remaining_bytes / mean_frame_bytes,
        exact: false,
    })
}

/// Pure policy: pipelined at or above `min_blobs`, walker below. Callers pass
/// [`crate::tunables::FULL_SCAN_ARM_MIN_BLOBS`]; tests inject a threshold a small fixture can
/// cross.
pub(crate) fn choose_scan_arm_at(estimate: &BlobCountEstimate, min_blobs: u64) -> ScanArm {
    if estimate.osmdata_blobs >= min_blobs {
        ScanArm::FullScan
    } else {
        ScanArm::Walker
    }
}

/// Per-blob metadata produced by [`HeaderWalker::next_header`].
pub(crate) struct BlobHeaderMeta {
    pub blob_type: BlobKind,
    /// Byte offset of the blob frame in the file (start of the 4-byte
    /// length prefix). Callers that want to `pread` the full frame for
    /// raw passthrough use `(frame_start, frame_size)`.
    pub frame_start: u64,
    /// 0-based index of this blob in the file. The walker owns provenance:
    /// work items built from this meta carry `(blob_index, frame_start)` so
    /// a worker that decodes the blob away from the walker can locate its
    /// errors truthfully.
    pub blob_index: u64,
    /// Byte offset in the file where the blob's Blob protobuf starts
    /// (i.e. the data payload after the 4-byte length prefix + header).
    pub data_offset: u64,
    /// Length of the blob's data payload in bytes.
    pub data_size: usize,
    /// Parsed indexdata, if the blob carried any.
    pub index: Option<BlobIndex>,
    /// Raw tagdata bytes from the BlobHeader, if present. Callers that
    /// need per-blob tag-index filtering (e.g. `tags-filter`'s two-pass
    /// schedule scans) deserialise these into a `TagIndex` on demand.
    pub tagdata: Option<Box<[u8]>>,
    /// Total frame size: 4 + header_len + data_size.
    pub frame_size: usize,
}

/// The walker's byte source: `pread` at the walk's position on the shared
/// file, bounded by the size captured at open.
///
/// The I/O shape is the measured one. Each frame starts with one probe
/// `pread` of up to [`HEADER_PROBE_SIZE`] bytes covering the length prefix
/// and (in the common case) the whole header; a header past the probe window
/// costs one more `pread` for exactly its remainder. A payload is never read:
/// skipping it is a check of its extent against the captured size, and drops
/// the probe window, so every frame gets a fresh probe.
///
/// Bytes inside the captured extent are committed: a file that shrank after
/// open fails the `pread` (`UnexpectedEof`, an error) rather than reading as
/// a shorter file. Only a captured remainder of 0-3 bytes at a frame boundary
/// is the clean tail, and it is recognised without reading it.
pub(crate) struct PreadSource {
    file: Arc<std::fs::File>,
    pos: u64,
    file_size: u64,
    /// The current frame's probe: `window_len` bytes of the file read at
    /// `window_at`.
    window: Vec<u8>,
    window_at: u64,
    window_len: usize,
}

impl PreadSource {
    /// The bytes left in the captured extent from the current position.
    fn remaining(&self) -> u64 {
        self.file_size.saturating_sub(self.pos)
    }

    /// The window bytes from the current position on, if it lies inside the
    /// window.
    fn windowed(&self) -> &[u8] {
        match self.pos.checked_sub(self.window_at) {
            Some(skip) if skip < self.window_len as u64 => {
                // skip < window_len, a usize.
                #[allow(clippy::cast_possible_truncation)]
                let skip = skip as usize;
                &self.window[skip..self.window_len]
            }
            _ => &[],
        }
    }
}

impl crate::frame::FrameSource for PreadSource {
    fn fill(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use std::os::unix::fs::FileExt as _;
        let remaining = self.remaining();
        if remaining < buf.len() as u64 && self.windowed().is_empty() {
            // The clean tail (or the end): not read, as the walk always did.
            return Ok(0);
        }
        if self.windowed().is_empty() {
            // A frame boundary: one probe pread covering the prefix and,
            // usually, the whole header.
            let probe = usize::try_from(remaining)
                .unwrap_or(usize::MAX)
                .min(HEADER_PROBE_SIZE);
            self.window.resize(probe, 0);
            self.file.read_exact_at(&mut self.window, self.pos)?;
            self.window_at = self.pos;
            self.window_len = probe;
        }
        let have = self.windowed();
        let n = have.len().min(buf.len());
        buf[..n].copy_from_slice(&have[..n]);
        self.pos += n as u64;
        Ok(n)
    }

    fn append(&mut self, n: usize, out: &mut Vec<u8>) -> std::io::Result<usize> {
        use std::os::unix::fs::FileExt as _;
        let take = usize::try_from(self.remaining())
            .unwrap_or(usize::MAX)
            .min(n);
        out.reserve(take);
        let have = self.windowed();
        let from_window = have.len().min(take);
        out.extend_from_slice(&have[..from_window]);
        self.pos += from_window as u64;
        let rest = take - from_window;
        if rest > 0 {
            // The part past the probe window, in one pread.
            let start = out.len();
            out.resize(start + rest, 0);
            if let Err(e) = self.file.read_exact_at(&mut out[start..], self.pos) {
                out.truncate(start);
                return Err(e);
            }
            self.pos += rest as u64;
        }
        Ok(take)
    }
}

impl crate::frame::FrameSkip for PreadSource {
    fn skip_bytes(&mut self, n: u64) -> std::io::Result<bool> {
        self.window_len = 0;
        if n > self.remaining() {
            return Ok(false);
        }
        self.pos += n;
        Ok(true)
    }
}

/// Walks blob headers via `pread`. Call [`Self::next_header`] repeatedly
/// until it returns `None`. Use [`Self::pread_data`] to fetch a blob's
/// data payload on demand.
pub(crate) struct HeaderWalker {
    /// The frame walk over the file. It owns the frame policy and locates
    /// every error it raises at the frame, naming this walker's file: a byte
    /// offset on a multi-input command does not say which input it indexes,
    /// and this walker is the preflight that most multi-input commands fail
    /// in first. A tripped cancellation token ends the walk with
    /// `ErrorKind::Cancelled`, unlocated.
    frames: Frames<PreadSource>,
    diagnostics: crate::host::Diagnostics,
    reported_unindexed_filter: bool,
    file: Arc<std::fs::File>,
    /// The file this walker was opened from.
    path: Arc<Path>,
    file_size: u64,
    /// Present when this walk refuses a partially indexed file; see
    /// [`Self::refusing_partial_index`].
    partial_index: Option<PartialIndexGuard<(u64, u64)>>,
}

/// The partially-indexed rule, one owner for every reader that enforces it
/// (the state behind [`HeaderWalker::refusing_partial_index`], and `digest`,
/// which checks it in its decoding pass): what the first data blob said
/// about indexdata, and where the first data blob without it sits. `L` is
/// that location, in whatever form the reader knows it. Fed every data
/// blob in file order; a file is partial once it has shown both kinds, and
/// the refusal names the first unindexed data blob, whether it came before
/// or after the first indexed one.
pub(crate) struct PartialIndexGuard<L> {
    /// Whether the first data blob carried readable indexdata; `None` until
    /// the walk has seen one.
    first_indexed: Option<bool>,
    /// Where the first data blob without readable indexdata sits; `None`
    /// until one has been seen (an unknown location is an `L` the caller
    /// chose, never a missing entry).
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
/// it at the blob the guard returned.
pub(crate) fn partial_index_error() -> crate::error::Error {
    crate::error::new_partial_indexdata(
        "the file is partially indexed: indexdata on some data blobs and not \
         others, and this is the first data blob without indexdata",
    )
}

impl HeaderWalker {
    /// Open `source` (a new open file description of it, so the advice
    /// below stays this walker's) and hint `posix_fadvise(RANDOM)` to the
    /// kernel.
    /// Errors if the file cannot be opened or metadata read.
    ///
    /// Intentionally opens a plain buffered fd regardless of whether
    /// the CLI caller passed `--direct-io`. Header walking is a
    /// tiny-read pattern where direct I/O would mean one aligned
    /// page-sized pread per header; buffered reads amortise that
    /// overhead. Callers that want direct I/O for the data-path
    /// body reads open their own worker fds alongside the walker's
    /// `shared_file()`, rather than flowing `--direct-io` through
    /// this helper.
    pub(crate) fn open<'a>(
        source: impl Into<crate::read::input::InputSource<'a>>,
        host: &crate::host::Host,
    ) -> Result<Self> {
        let source = source.into();
        let path = source.path();
        let file = source.open()?;
        let file_size = file
            .metadata()
            .map_err(|e| {
                crate::Error::from(e).context(format!("failed to stat {}", path.display()))
            })?
            .len();
        // POSIX_FADV_RANDOM: suppresses readahead. Our pread pattern is
        // header-at-a-time (tiny reads) with big gaps, plus occasional
        // full-blob body preads that are also not in a sequential stream
        // (workers may address disjoint shards). Sequential readahead
        // would pull blob bodies adjacent to each header into the page
        // cache and never be reused.
        crate::fadvise::advise(&file, 0, 0, crate::fadvise::Advice::Random);
        let file = Arc::new(file);
        let source = PreadSource {
            file: Arc::clone(&file),
            pos: 0,
            file_size,
            window: Vec::new(),
            window_at: 0,
            window_len: 0,
        };
        Ok(Self {
            frames: Frames::of_input(source, path, host),
            diagnostics: host.diagnostics.clone(),
            reported_unindexed_filter: false,
            file,
            path: Arc::from(path),
            file_size,
            partial_index: None,
        })
    }

    /// Make this walk refuse a partially indexed file: once the data blobs
    /// seen so far include one with readable indexdata and one without,
    /// [`Self::next_header`] returns the typed missing-indexdata error
    /// located at the first data blob without it. Indexdata that is present
    /// but unreadable counts as absent, as it does for every consumer. A
    /// wholly indexed file and a wholly unindexed one both pass; whether a
    /// wholly unindexed one is acceptable stays the caller's first-blob gate.
    ///
    /// For walks that already visit every header to build a command's
    /// schedule, so the refusal costs no extra read. The first-blob probe
    /// (`has_indexdata`) is what decides elsewhere whether a file is indexed,
    /// so a file indexed on its first blob and not on a later one would
    /// otherwise pass as indexed and run on an index it does not have.
    /// Walks that do not build a schedule (inspect's description, the blob
    /// count probe) do not opt in. Nor do getid's: every getid path decodes
    /// a blob without indexdata and filters its actual elements, so a
    /// partially indexed file is correct input for it and is accepted
    /// (`scan::classify::PartialIndex::Accept` on its classify walks).
    /// getparents' walker does not opt in for the same reason: it schedules
    /// a blob without indexdata whatever the query, and its full-scan arm's
    /// blob filter passes one through to a decode, so both arms select from
    /// actual elements. tags-filter's pass 1 and pass 2 walks do not opt in
    /// either: each schedules a blob without indexdata and matches or writes
    /// its decoded elements by their actual kind and ID.
    #[must_use]
    pub(crate) fn refusing_partial_index(mut self) -> Self {
        self.partial_index = Some(PartialIndexGuard::default());
        self
    }

    /// Report a filtered schedule retaining an unindexed data blob. Call only
    /// where a schedule uses type or spatial index pruning, not on every walk.
    /// Mixed indexed/unindexed files also need this note: later missing indexes
    /// disable pruning even when the first data blob was indexed.
    pub(crate) fn observe_filter_index(&mut self, index: Option<&BlobIndex>) {
        if index.is_some() || self.reported_unindexed_filter {
            return;
        }
        self.reported_unindexed_filter = true;
        crate::host::diag_once!(
            self.diagnostics,
            Note,
            "reader.filter_without_indexdata",
            "Input contains blobs without indexdata; blob type and spatial filters cannot skip those blobs"
        );
        crate::debug::emit_counter("schedule_filter_without_indexdata", 1);
    }

    /// The path this walker was opened from. Workers that decode blobs the
    /// walker scheduled capture this once and locate their errors with it.
    pub(crate) fn path(&self) -> &Arc<Path> {
        &self.path
    }

    /// Attach the provenance this walker handed out in `meta` (the input,
    /// the blob's true index and its frame start) to an error raised while
    /// working on that blob. Applied only when the error has no location yet.
    #[cold]
    pub(crate) fn locate_meta(
        &self,
        meta: &BlobHeaderMeta,
        err: crate::error::Error,
    ) -> crate::error::Error {
        err.locate(crate::error::Location::new(
            Some(&self.path),
            Some(meta.blob_index),
            Some(crate::ByteOffset(meta.frame_start)),
        ))
    }

    /// The walker's file handle and path, for the pools that pread the blobs
    /// this walk scheduled and locate their failures in this input.
    pub(crate) fn input(&self) -> crate::read::input::InputFile {
        crate::read::input::InputFile::from_parts(Arc::clone(&self.file), Arc::clone(&self.path))
    }

    /// Shared `Arc<File>` for worker threads to `pread` data bodies.
    /// Used when the walker is handed off as a plan producer and the
    /// actual body reads happen on rayon workers (e.g. the diff
    /// parallel shard path, or the shared `scan::classify`
    /// schedule builders). Single-threaded callers ignore this.
    pub(crate) fn shared_file(&self) -> &Arc<std::fs::File> {
        &self.file
    }

    /// Total file size captured at `open()` time.
    ///
    /// The walk owns the payload-extent check: [`Self::next_header`] refuses
    /// a frame whose declared payload runs past this size, located at that
    /// frame, so every meta it returns already satisfies
    /// `data_offset + data_size <= file_size`. Callers must not repeat the
    /// check before a pread; a copy can only drift from the walk's.
    pub(crate) fn file_size(&self) -> u64 {
        self.file_size
    }

    /// Read the next blob's header and advance the internal offset past the
    /// data payload without reading it. Returns `None` at EOF.
    ///
    /// Issues a single probe `pread` of up to [`HEADER_PROBE_SIZE`] bytes
    /// covering the 4-byte length prefix plus the blob header in the common
    /// case (typical headers run ~100-200 B; indexed blobs with tagdata a
    /// bit more). A second `pread` is only needed for the rare header that
    /// extends past the probe window. The payload is not read: the walk
    /// refuses a frame whose declared payload runs past the file, located at
    /// that frame, so a truncated tail blob cannot end the walk silently.
    pub(crate) fn next_header(&mut self) -> Result<Option<BlobHeaderMeta>> {
        let Some(header) = self.frames.next_header()? else {
            return Ok(None);
        };
        // The frame's declared datasize is bounded by `MAX_BLOB_DATASIZE`, so
        // every `pread_data(data_offset, data_size, ..)` a consumer later
        // issues against this meta is bounded too.
        self.frames.skip_payload()?;
        // A walk from offset 0 that has not seeked always knows both.
        let frame_start = header.id.start.unwrap_or(0);
        let blob_index = header.id.ordinal.unwrap_or(0);
        let payload_offset = header.payload_offset();
        let crate::blob::WireBlobHeader {
            blob_type,
            indexdata,
            tagdata,
            ..
        } = header.header;
        let meta = BlobHeaderMeta {
            blob_type,
            frame_start,
            blob_index,
            data_offset: frame_start + payload_offset as u64,
            data_size: header.data_size,
            index: indexdata.and_then(|b| BlobIndex::deserialize(&b)),
            tagdata,
            frame_size: payload_offset + header.data_size,
        };
        // The partial-index refusal names the first unindexed data blob,
        // which may be an earlier frame than this one, so it is located here
        // and not by the frame walk. It ends the walk like a frame failure.
        if meta.blob_type == BlobKind::OsmData
            && let Some(guard) = self.partial_index.as_mut()
            && let Some((blob, frame_start)) =
                guard.observe(meta.index.is_some(), (meta.blob_index, meta.frame_start))
        {
            self.frames.poison();
            return Err(partial_index_error().locate(crate::error::Location::new(
                Some(&self.path),
                Some(blob),
                Some(crate::ByteOffset(frame_start)),
            )));
        }
        Ok(Some(meta))
    }

    /// `pread` a blob's data payload into `buf`. Resizes `buf` to `size`.
    pub(crate) fn pread_data(&self, offset: u64, size: usize, buf: &mut Vec<u8>) -> Result<()> {
        use std::os::unix::fs::FileExt as _;
        buf.resize(size, 0);
        Ok(self.file.read_exact_at(buf, offset)?)
    }
}

#[cfg(test)]
mod tests {
    use super::{BlobCountEstimate, SAMPLE_CAP, ScanArm, choose_scan_arm_at, estimate_blob_count};
    use crate::block_builder::{BlockBuilder, HeaderBuilder};
    use crate::tunables::FULL_SCAN_ARM_MIN_BLOBS;
    use crate::writer::{Compression, PbfWriter};

    #[test]
    fn filtered_walk_reports_missing_indexes_once_across_walkers() {
        #[derive(Default)]
        struct Sink(std::sync::Mutex<Vec<&'static str>>);
        impl crate::host::DiagnosticSink for Sink {
            fn event(&self, event: &crate::host::Event<'_>) {
                self.0.lock().expect("sink").push(event.key);
            }
        }

        let dir = crate::path_guard::tests::scratch();
        let path = dir.path().join("filtered.pbf");
        write_fixture(&path, true);
        let sink = std::sync::Arc::new(Sink::default());
        let diagnostics = crate::host::Diagnostics::new({
            let shared: std::sync::Arc<Sink> = std::sync::Arc::clone(&sink);
            shared
        });
        let index = crate::blob_meta::BlobIndex {
            kind: crate::ElementKind::Node,
            min_id: 1,
            max_id: 1,
            count: 1,
            bbox: None,
        };
        let host = crate::host::Host {
            diagnostics,
            ..crate::host::Host::for_tests()
        };
        let mut first = super::HeaderWalker::open(&path, &host).expect("walker");
        first.observe_filter_index(Some(&index));
        assert!(sink.0.lock().expect("sink").is_empty());
        // A later missing index must report even after an indexed first blob.
        first.observe_filter_index(None);
        first.observe_filter_index(None);
        let mut second = super::HeaderWalker::open(&path, &host).expect("walker");
        second.observe_filter_index(None);
        assert_eq!(
            *sink.0.lock().expect("sink"),
            ["reader.filter_without_indexdata"]
        );
        assert!(first.reported_unindexed_filter);
        assert!(second.reported_unindexed_filter);
    }

    #[test]
    fn open_preserves_not_found_error_kind() {
        let scratch = crate::path_guard::tests::scratch();
        let missing = scratch.path().join("missing.pbf");
        let result = super::HeaderWalker::open(&missing, &crate::host::Host::for_tests());

        match result {
            Ok(_) => panic!("opening a missing file must fail"),
            Err(error) => assert!(matches!(
                error.kind(),
                crate::error::ErrorKind::Io(io_error)
                    if io_error.kind() == std::io::ErrorKind::NotFound
            )),
        }
    }

    fn write_fixture(path: &std::path::Path, with_data: bool) {
        let file = std::fs::File::create(path).expect("create fixture");
        let mut writer = PbfWriter::new(
            std::io::BufWriter::with_capacity(crate::write::IO_BUF_BYTES, file),
            Compression::default(),
        );
        writer
            .write_header(&HeaderBuilder::new().build().expect("header"))
            .expect("write header");
        if with_data {
            let mut block = BlockBuilder::new();
            block.add_node(1, 0, 0, std::iter::empty::<(&str, &str)>(), None);
            writer
                .write_primitive_block(block.take().expect("take").expect("block"))
                .expect("write block");
        }
        writer.flush().expect("flush fixture");
    }

    /// A header, then one single-node data blob per entry of `indexed`,
    /// carrying indexdata where the entry is true.
    fn write_index_pattern(path: &std::path::Path, indexed: &[bool]) {
        let file = std::fs::File::create(path).expect("create fixture");
        let mut writer = PbfWriter::new(
            std::io::BufWriter::with_capacity(crate::write::IO_BUF_BYTES, file),
            Compression::default(),
        );
        writer
            .write_header(&HeaderBuilder::new().build().expect("header"))
            .expect("write header");
        let mut block = BlockBuilder::new();
        for (id, &with_index) in (1_i64..).zip(indexed) {
            block.add_node(id, 0, 0, std::iter::empty::<(&str, &str)>(), None);
            let bytes = block.take().expect("take").expect("block");
            if with_index {
                writer.write_primitive_block(bytes).expect("write block");
            } else {
                writer
                    .write_primitive_block_no_indexdata(bytes)
                    .expect("write block");
            }
        }
        writer.flush().expect("flush fixture");
    }

    fn walk_all(path: &std::path::Path, refusing: bool) -> super::Result<usize> {
        let mut walker = super::HeaderWalker::open(path, &crate::host::Host::for_tests())?;
        if refusing {
            walker = walker.refusing_partial_index();
        }
        let mut n = 0;
        while walker.next_header()?.is_some() {
            n += 1;
        }
        Ok(n)
    }

    /// A partially indexed file is the typed missing-indexdata error, located
    /// at the first data blob without indexdata, whichever kind comes first.
    /// Uniform files pass, and a walk that did not opt in never refuses.
    #[test]
    fn a_refusing_walk_stops_a_partially_indexed_file_at_its_first_unindexed_blob() {
        let dir = crate::path_guard::tests::scratch();
        // (pattern, blob ordinal of the first unindexed data blob; the header
        // is blob 0).
        for (pattern, first_unindexed) in [
            (&[true, true, false, true][..], 3_u64),
            (&[false, true][..], 1),
            (&[false, false, true][..], 1),
        ] {
            let path = dir.path().join("partial.pbf");
            write_index_pattern(&path, pattern);
            let err = walk_all(&path, true).expect_err("a partial index must be refused");
            assert!(
                matches!(err.kind(), crate::error::ErrorKind::MissingIndexdata { .. }),
                "{pattern:?}: {err:?}"
            );
            assert_eq!(err.blob_index(), Some(first_unindexed), "{pattern:?}");
            assert_eq!(err.source_path(), Some(path.as_path()), "{pattern:?}");
            assert_eq!(
                walk_all(&path, false).expect("an ordinary walk passes"),
                pattern.len() + 1,
                "{pattern:?}"
            );
        }
        // The refusal ends the walk: it does not resume at the next frame.
        let path = dir.path().join("partial-stop.pbf");
        write_index_pattern(&path, &[true, false, true]);
        let mut walker = super::HeaderWalker::open(&path, &crate::host::Host::for_tests())
            .expect("walker")
            .refusing_partial_index();
        while let Ok(Some(_)) = walker.next_header() {}
        assert!(walker.next_header().is_err(), "a refused walk resumed");

        for pattern in [&[true, true][..], &[false, false][..], &[][..]] {
            let path = dir.path().join("uniform.pbf");
            write_index_pattern(&path, pattern);
            assert_eq!(
                walk_all(&path, true).expect("a uniform file passes"),
                pattern.len() + 1,
                "{pattern:?}"
            );
        }
    }

    /// The shared EOF policy through the pread source, which recognises the
    /// tolerated tail from the captured file size without reading it.
    ///
    /// `read::frame` owns one policy - a 0-3 byte tail at a frame boundary is
    /// clean EOF, a complete 4-byte prefix is a committed frame - and
    /// `PreadSource::fill` is where the walk decides how short the tail is,
    /// so a unit test over a stream source cannot reach it. Without this
    /// test the walk could drift back to erroring on a tolerated tail, or to
    /// swallowing a committed frame as EOF, with both gates green.
    #[test]
    fn the_walk_tolerates_a_short_tail_but_not_a_complete_prefix() {
        let dir = crate::path_guard::tests::scratch();
        let base = dir.path().join("base.pbf");
        write_fixture(&base, true);
        let good = std::fs::read(&base).expect("read fixture");

        let count_headers = |path: &std::path::Path| -> super::Result<usize> {
            let mut walker = super::HeaderWalker::open(path, &crate::host::Host::for_tests())?;
            let mut n = 0;
            while walker.next_header()?.is_some() {
                n += 1;
            }
            Ok(n)
        };

        let baseline = count_headers(&base).expect("the intact fixture walks");
        assert!(baseline >= 2, "fixture must have header + data blobs");

        for tail in 0..4usize {
            let path = dir.path().join(format!("tail{tail}.pbf"));
            let mut bytes = good.clone();
            bytes.extend(std::iter::repeat_n(0xAAu8, tail));
            std::fs::write(&path, &bytes).expect("write tail fixture");
            assert_eq!(
                count_headers(&path).unwrap_or_else(|e| panic!("{tail}-byte tail: {e:?}")),
                baseline,
                "a {tail}-byte tail must read as clean EOF after every real blob"
            );
        }

        // A complete prefix is a committed frame. 0xFFFFFFFF is the
        // unbounded-allocation case `check_header_len` exists for.
        let path = dir.path().join("tail4.pbf");
        let mut bytes = good.clone();
        bytes.extend_from_slice(&[0xFFu8; 4]);
        std::fs::write(&path, &bytes).expect("write committed-frame fixture");
        count_headers(&path).expect_err("a complete prefix must not be excused as EOF");
    }

    #[test]
    fn chooser_switches_at_the_policy_boundary() {
        assert_eq!(
            choose_scan_arm_at(
                &BlobCountEstimate {
                    osmdata_blobs: FULL_SCAN_ARM_MIN_BLOBS - 1,
                    exact: true,
                },
                FULL_SCAN_ARM_MIN_BLOBS
            ),
            ScanArm::Walker
        );
        assert_eq!(
            choose_scan_arm_at(
                &BlobCountEstimate {
                    osmdata_blobs: FULL_SCAN_ARM_MIN_BLOBS,
                    exact: false,
                },
                FULL_SCAN_ARM_MIN_BLOBS
            ),
            ScanArm::FullScan
        );
    }

    #[test]
    fn chooser_is_independent_of_estimate_exactness() {
        let estimate = BlobCountEstimate {
            osmdata_blobs: 1,
            exact: false,
        };
        assert_eq!(choose_scan_arm_at(&estimate, 1), ScanArm::FullScan);
    }

    #[test]
    fn estimator_is_exact_for_small_and_header_only_files() {
        let dir = crate::path_guard::tests::scratch();
        let header_only = dir.path().join("header-only.pbf");
        write_fixture(&header_only, false);
        assert_eq!(
            estimate_blob_count(&header_only, &crate::host::Host::for_tests()).expect("estimate"),
            BlobCountEstimate {
                osmdata_blobs: 0,
                exact: true,
            }
        );

        let one_block = dir.path().join("one-block.pbf");
        write_fixture(&one_block, true);
        assert_eq!(
            estimate_blob_count(&one_block, &crate::host::Host::for_tests()).expect("estimate"),
            BlobCountEstimate {
                osmdata_blobs: 1,
                exact: true,
            }
        );
    }

    #[test]
    fn estimator_rejects_empty_input() {
        let dir = crate::path_guard::tests::scratch();
        let empty = dir.path().join("empty.pbf");
        std::fs::File::create(&empty).expect("create empty file");
        assert!(estimate_blob_count(&empty, &crate::host::Host::for_tests()).is_err());
    }

    #[test]
    fn estimator_projects_within_tolerance_on_mixed_frame_sizes() {
        let dir = crate::path_guard::tests::scratch();
        let mixed = dir.path().join("mixed.pbf");
        let file = std::fs::File::create(&mixed).expect("create fixture");
        let mut writer = PbfWriter::new(
            std::io::BufWriter::with_capacity(crate::write::IO_BUF_BYTES, file),
            Compression::None,
        );
        writer
            .write_header(&HeaderBuilder::new().build().expect("header"))
            .expect("write header");
        // Alternate small and large frames so the projected mean has to
        // absorb per-frame size variance, and overshoot the sample cap so
        // the estimation branch (not the exact count) is exercised.
        let actual_blobs = (SAMPLE_CAP + SAMPLE_CAP / 2) as u64;
        let mut block = BlockBuilder::new();
        for blob in 0..actual_blobs {
            let nodes_in_blob = if blob % 2 == 0 { 1 } else { 40 };
            for node in 0..nodes_in_blob {
                #[allow(clippy::cast_possible_wrap)]
                let id = (blob * 64 + node) as i64;
                block.add_node(
                    id,
                    0,
                    0,
                    [("highway", "primary_link")].iter().copied(),
                    None,
                );
            }
            writer
                .write_primitive_block(block.take().expect("take").expect("block"))
                .expect("write block");
        }
        writer.flush().expect("flush fixture");

        let estimate =
            estimate_blob_count(&mixed, &crate::host::Host::for_tests()).expect("estimate");
        assert!(!estimate.exact);
        let error = estimate.osmdata_blobs.abs_diff(actual_blobs);
        assert!(
            error * 100 < actual_blobs * 30,
            "estimated {} of {actual_blobs} actual blobs; relative error over 30 %",
            estimate.osmdata_blobs,
        );
    }
}
