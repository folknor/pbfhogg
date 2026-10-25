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

use crate::blob::{BlobKind, parse_blob_header_with_index};
use crate::blob_meta::BlobIndex;
use crate::error::Result;
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
pub(crate) fn estimate_blob_count(
    path: &Path,
    host: &crate::host::Host,
) -> Result<BlobCountEstimate> {
    let mut walker = HeaderWalker::open(path, host)?;
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

/// Walks blob headers via `pread`. Call [`Self::next_header`] repeatedly
/// until it returns `None`. Use [`Self::pread_data`] to fetch a blob's
/// data payload on demand.
pub(crate) struct HeaderWalker {
    diagnostics: crate::host::Diagnostics,
    /// Checked before every header: a tripped token ends the walk with
    /// `ErrorKind::Cancelled`.
    cancel: crate::host::Cancel,
    reported_unindexed_filter: bool,
    file: Arc<std::fs::File>,
    /// The file this walker was opened from. Stamped onto every error it
    /// raises, for the same reason `BlobReader` keeps one: a byte offset
    /// on a multi-input command does not say which input it indexes, and
    /// this walker is the preflight that most multi-input commands fail
    /// in first.
    path: Arc<Path>,
    offset: u64,
    file_size: u64,
    header_buf: Vec<u8>,
    /// 0-based index of the blob whose header was last returned.
    blob_index: u64,
    /// Present when this walk refuses a partially indexed file; see
    /// [`Self::refusing_partial_index`].
    partial_index: Option<PartialIndexGuard>,
}

/// The state behind [`HeaderWalker::refusing_partial_index`]: what the first
/// data blob said about indexdata, and where the first data blob without it
/// sits.
#[derive(Default)]
struct PartialIndexGuard {
    /// Whether the first data blob carried readable indexdata; `None` until
    /// the walk has seen one.
    first_indexed: Option<bool>,
    /// `(blob ordinal, frame start)` of the first data blob without readable
    /// indexdata.
    first_unindexed: Option<(u64, u64)>,
}

impl PartialIndexGuard {
    /// Record one data blob. Returns the location of the first unindexed
    /// data blob once the file has shown both kinds.
    fn observe(&mut self, meta: &BlobHeaderMeta) -> Option<(u64, u64)> {
        let indexed = meta.index.is_some();
        if !indexed && self.first_unindexed.is_none() {
            self.first_unindexed = Some((meta.blob_index, meta.frame_start));
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
        Ok(Self {
            diagnostics: host.diagnostics.clone(),
            cancel: host.cancel.clone(),
            reported_unindexed_filter: false,
            file: Arc::new(file),
            path: Arc::from(path),
            offset: 0,
            file_size,
            header_buf: Vec::new(),
            blob_index: 0,
            partial_index: None,
        })
    }

    /// Start the walk at the frame beginning at byte `frame_start`, the
    /// `blob_index`-th frame of the file, instead of at the file's start.
    /// For a caller that read the file this far through another reader and
    /// hands the rest to a header-only walk; both values must be that
    /// reader's logical position (a frame boundary), not where an underlying
    /// descriptor has read ahead to. Errors keep naming the true ordinal and
    /// offset, and a leading-header blob found here is refused as misplaced.
    #[must_use]
    pub(crate) fn resume_at(mut self, frame_start: u64, blob_index: u64) -> Self {
        self.offset = frame_start;
        self.blob_index = blob_index;
        self
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

    /// Attach this walker's input identity, the blob it was reading and that
    /// blob's frame start to an error, as one location. Every error path
    /// out of `next_header` goes through here; one that does not makes
    /// `Error::source_path` lie by omission on that path only, which is
    /// worse than never having had it. `self.offset` is the frame start
    /// here: it only advances once a frame has been fully validated.
    fn locate(&self, err: crate::error::Error) -> crate::error::Error {
        err.locate(crate::error::Location::new(
            Some(&self.path),
            Some(self.blob_index),
            Some(crate::ByteOffset(self.offset)),
        ))
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
    /// extends past the probe window.
    pub(crate) fn next_header(&mut self) -> Result<Option<BlobHeaderMeta>> {
        // A cancellation is not a fault of the input: returned unlocated.
        self.cancel.check()?;
        // One funnel for both the success counter and the error stamp, so
        // no early return inside the walk can escape unattributed.
        match self.next_header_inner() {
            Ok(Some(meta)) => {
                self.blob_index += 1;
                if meta.blob_type == BlobKind::OsmData
                    && let Some(guard) = self.partial_index.as_mut()
                    && let Some((blob, frame_start)) = guard.observe(&meta)
                {
                    return Err(crate::error::new_partial_indexdata(
                        "the file is partially indexed: indexdata on some data blobs and not \
                         others, and this is the first data blob without indexdata",
                    )
                    .locate(crate::error::Location::new(
                        Some(&self.path),
                        Some(blob),
                        Some(crate::ByteOffset(frame_start)),
                    )));
                }
                Ok(Some(meta))
            }
            Ok(None) => Ok(None),
            Err(err) => Err(self.locate(err)),
        }
    }

    fn next_header_inner(&mut self) -> Result<Option<BlobHeaderMeta>> {
        use std::os::unix::fs::FileExt as _;

        if self.offset >= self.file_size {
            return Ok(None);
        }
        let frame_start = self.offset;

        // Probe: one pread covering length prefix + (usually) full header.
        let remaining = self.file_size - self.offset;
        let probe_len = usize::try_from(remaining)
            .unwrap_or(usize::MAX)
            .min(HEADER_PROBE_SIZE);
        if probe_len < 4 {
            // Not enough bytes left to parse a length prefix; treat as
            // clean EOF, matching prior UnexpectedEof-tolerant behavior.
            return Ok(None);
        }
        self.header_buf.resize(probe_len, 0);
        // Per `reference/truncation-handling.md`: a committed read past
        // EOF (i.e. probe_len bytes promised by `remaining` derived from
        // `file_size`) is shape 2 or 3 - hard error, not silent EOF.
        // The clean-EOF case is handled above at the `probe_len < 4`
        // guard.
        self.file.read_exact_at(&mut self.header_buf, self.offset)?;

        let header_len = u32::from_be_bytes([
            self.header_buf[0],
            self.header_buf[1],
            self.header_buf[2],
            self.header_buf[3],
        ]);
        // Both length-prefix guards come from `read::frame`, the one owner of
        // frame policy. The walk preads rather than streams, so it shares the
        // policy but not the I/O.
        let header_len = crate::frame::check_header_len(header_len)?;
        let header_end = 4 + header_len;

        if header_end > probe_len {
            // Fallback: header extends past the probe window. Top up with a
            // second pread for just the tail.
            self.header_buf.resize(header_end, 0);
            let tail_offset = self.offset + probe_len as u64;
            self.file
                .read_exact_at(&mut self.header_buf[probe_len..header_end], tail_offset)?;
        }

        // parse_blob_header_with_index rejects an oversized declared datasize
        // (>= MAX_BLOB_DATASIZE) before returning, so the `data_size` below -
        // and every `pread_data(data_offset, data_size, ..)` a consumer later
        // issues against it - is bounded, never a hostile multi-GB size.
        let (blob_type, data_size, raw_index, _index_present, tagdata, _tagdata_present) =
            parse_blob_header_with_index(
                &self.header_buf[4..header_end],
                &self.diagnostics,
                crate::blob::BlobSite {
                    source: Some(&*self.path),
                    offset: Some(frame_start),
                },
            )?;
        crate::frame::check_header_position(&blob_type, frame_start == 0)?;
        let index = raw_index.as_ref().and_then(|b| BlobIndex::deserialize(b));

        let data_offset = self.offset + header_end as u64;
        // Per `reference/truncation-handling.md` shape 4: the declared
        // payload must fit within the file. Without this check, a
        // truncated tail blob would silently terminate the walk on the
        // next call (offset >= file_size returns Ok(None)).
        let payload_end = data_offset.checked_add(data_size as u64).ok_or_else(|| {
            crate::error::Error::invalid_data(format!(
                "blob at offset {} declares overflowing payload size {data_size}",
                self.offset
            ))
        })?;
        if payload_end > self.file_size {
            return Err(crate::error::Error::io_kind(
                ::std::io::ErrorKind::UnexpectedEof,
                format!(
                    "blob payload truncated: declared {data_size} bytes \
                     from offset {data_offset}, file_size {}",
                    self.file_size
                ),
            ));
        }
        self.offset = payload_end;
        let frame_size = 4 + header_len + data_size;

        Ok(Some(BlobHeaderMeta {
            blob_type,
            frame_start,
            blob_index: self.blob_index,
            data_offset,
            data_size,
            index,
            tagdata,
            frame_size,
        }))
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

    /// The walk's own spelling of the shared EOF policy, pinned at the call
    /// site rather than only in `read::frame`.
    ///
    /// `read::frame` owns one policy - a 0-3 byte tail at a frame boundary is
    /// clean EOF, a complete 4-byte prefix is a committed frame - but only the
    /// three streaming decoders share its I/O. `HeaderWalker` preads, so it
    /// spells the same rule as `probe_len < 4` and a unit test of the helper
    /// cannot reach it; `BlobReader`'s third spelling is pinned by
    /// `tests/read_paths.rs::trailing_partial_length_prefix_*`. Without this
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
