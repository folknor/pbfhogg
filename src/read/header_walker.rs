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

/// Size of the initial header probe `pread` per blob. One page covers the
/// 4-byte length prefix plus the header bytes for essentially every blob
/// in real PBFs; the fallback path in `next_header` handles the rare
/// exception where a blob's header exceeds the probe window.
const HEADER_PROBE_SIZE: usize = 4096;

/// Number of leading frames sampled when estimating a file's blob count.
const SAMPLE_CAP: usize = 1_000;

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

/// Blob-count policy boundary measured in `reference/blob-density.md`.
pub(crate) const FULL_SCAN_ARM_MIN_BLOBS: u64 = 150_000;

/// Estimate the number of OSMData frames using a bounded header-only probe.
///
/// The first header frame and unknown frames are intentionally excluded from
/// the frame-size mean: only OSMData frame bytes predict OSMData count.
pub(crate) fn estimate_blob_count(
    path: &Path,
    diagnostics: &crate::host::Diagnostics,
) -> Result<BlobCountEstimate> {
    let mut walker = HeaderWalker::open(path, diagnostics)?;
    let file_size = walker.file_size();
    if file_size == 0 {
        return Err(crate::error::new_error(
            crate::error::ErrorKind::MissingHeader,
        ));
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
            return Err(crate::error::new_error(
                crate::error::ErrorKind::MissingHeader,
            ));
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
/// [`FULL_SCAN_ARM_MIN_BLOBS`]; tests inject a threshold a small fixture can
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
}

impl HeaderWalker {
    /// Open `path` and hint `posix_fadvise(RANDOM)` to the kernel.
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
    pub(crate) fn open(path: &Path, diagnostics: &crate::host::Diagnostics) -> Result<Self> {
        let file = std::fs::File::open(path).map_err(|e| {
            crate::error::new_error(crate::error::ErrorKind::Io(super::file_reader::open_error(
                path, &e,
            )))
        })?;
        let file_size = file
            .metadata()
            .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?
            .len();
        // POSIX_FADV_RANDOM: suppresses readahead. Our pread pattern is
        // header-at-a-time (tiny reads) with big gaps, plus occasional
        // full-blob body preads that are also not in a sequential stream
        // (workers may address disjoint shards). Sequential readahead
        // would pull blob bodies adjacent to each header into the page
        // cache and never be reused.
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::io::AsRawFd;
            // Safety: posix_fadvise on a valid fd is safe to call; any
            // error is a hint that can be ignored (see posix_fadvise(2)).
            unsafe {
                libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_RANDOM);
            }
        }
        Ok(Self {
            diagnostics: diagnostics.clone(),
            file: Arc::new(file),
            path: Arc::from(path),
            offset: 0,
            file_size,
            header_buf: Vec::new(),
            blob_index: 0,
        })
    }

    /// Attach this walker's input identity to an error. Every error path
    /// out of `next_header` goes through here; one that does not makes
    /// `Error::source_path` lie by omission on that path only, which is
    /// worse than never having had it.
    fn stamp(&self, err: crate::error::Error) -> crate::error::Error {
        err.in_input(Some(&self.path), Some(self.blob_index))
    }

    /// Shared `Arc<File>` for worker threads to `pread` data bodies.
    /// Used when the walker is handed off as a plan producer and the
    /// actual body reads happen on rayon workers (e.g. the diff
    /// parallel shard path, or the shared `scan::classify`
    /// schedule builders). Single-threaded callers ignore this.
    pub(crate) fn shared_file(&self) -> &Arc<std::fs::File> {
        &self.file
    }

    /// Total file size captured at `open()` time. Callers can use this
    /// to validate per-blob `data_offset + data_size <= file_size`
    /// before handing a schedule to parallel pread workers, catching
    /// truncation / corrupt-header cases up front rather than at
    /// `read_exact_at`.
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
        // One funnel for both the success counter and the error stamp, so
        // no early return inside the walk can escape unattributed.
        match self.next_header_inner() {
            Ok(Some(meta)) => {
                self.blob_index += 1;
                Ok(Some(meta))
            }
            Ok(None) => Ok(None),
            Err(err) => Err(self.stamp(err)),
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
        self.file
            .read_exact_at(&mut self.header_buf, self.offset)
            .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?;

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
                .read_exact_at(&mut self.header_buf[probe_len..header_end], tail_offset)
                .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?;
        }

        // parse_blob_header_with_index rejects an oversized declared datasize
        // (>= MAX_BLOB_DATASIZE) before returning, so the `data_size` below -
        // and every `pread_data(data_offset, data_size, ..)` a consumer later
        // issues against it - is bounded, never a hostile multi-GB size.
        let (blob_type, data_size, raw_index, _index_present, tagdata, _tagdata_present) =
            parse_blob_header_with_index(&self.header_buf[4..header_end], &self.diagnostics)?;
        let index = raw_index.as_ref().and_then(|b| BlobIndex::deserialize(b));

        let data_offset = self.offset + header_end as u64;
        // Per `reference/truncation-handling.md` shape 4: the declared
        // payload must fit within the file. Without this check, a
        // truncated tail blob would silently terminate the walk on the
        // next call (offset >= file_size returns Ok(None)).
        let payload_end = data_offset.checked_add(data_size as u64).ok_or_else(|| {
            crate::error::new_error(crate::error::ErrorKind::Io(::std::io::Error::new(
                ::std::io::ErrorKind::InvalidData,
                format!(
                    "blob at offset {} declares overflowing payload size {data_size}",
                    self.offset
                ),
            )))
        })?;
        if payload_end > self.file_size {
            return Err(crate::error::new_error(crate::error::ErrorKind::Io(
                ::std::io::Error::new(
                    ::std::io::ErrorKind::UnexpectedEof,
                    format!(
                        "blob payload truncated: declared {data_size} bytes \
                         from offset {data_offset}, file_size {}",
                        self.file_size
                    ),
                ),
            )));
        }
        self.offset = payload_end;
        let frame_size = 4 + header_len + data_size;

        Ok(Some(BlobHeaderMeta {
            blob_type,
            frame_start,
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
        self.file
            .read_exact_at(buf, offset)
            .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BlobCountEstimate, FULL_SCAN_ARM_MIN_BLOBS, SAMPLE_CAP, ScanArm, choose_scan_arm_at,
        estimate_blob_count,
    };
    use crate::block_builder::{BlockBuilder, HeaderBuilder};
    use crate::writer::{Compression, PbfWriter};

    #[test]
    fn open_preserves_not_found_error_kind() {
        let missing = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("data")
            .join("bench-tmp")
            .join(format!("missing-header-walker-{}.pbf", std::process::id()));
        let result = super::HeaderWalker::open(&missing, &crate::host::Diagnostics::discard());

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
        let mut writer = PbfWriter::new(std::io::BufWriter::new(file), Compression::default());
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
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().join("base.pbf");
        write_fixture(&base, true);
        let good = std::fs::read(&base).expect("read fixture");

        let count_headers = |path: &std::path::Path| -> super::Result<usize> {
            let mut walker = super::HeaderWalker::open(path, &crate::host::Diagnostics::discard())?;
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
        let dir = tempfile::tempdir().expect("tempdir");
        let header_only = dir.path().join("header-only.pbf");
        write_fixture(&header_only, false);
        assert_eq!(
            estimate_blob_count(&header_only, &crate::host::Diagnostics::discard())
                .expect("estimate"),
            BlobCountEstimate {
                osmdata_blobs: 0,
                exact: true,
            }
        );

        let one_block = dir.path().join("one-block.pbf");
        write_fixture(&one_block, true);
        assert_eq!(
            estimate_blob_count(&one_block, &crate::host::Diagnostics::discard())
                .expect("estimate"),
            BlobCountEstimate {
                osmdata_blobs: 1,
                exact: true,
            }
        );
    }

    #[test]
    fn estimator_rejects_empty_input() {
        let dir = tempfile::tempdir().expect("tempdir");
        let empty = dir.path().join("empty.pbf");
        std::fs::File::create(&empty).expect("create empty file");
        assert!(estimate_blob_count(&empty, &crate::host::Diagnostics::discard()).is_err());
    }

    #[test]
    fn estimator_projects_within_tolerance_on_mixed_frame_sizes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mixed = dir.path().join("mixed.pbf");
        let file = std::fs::File::create(&mixed).expect("create fixture");
        let mut writer = PbfWriter::new(std::io::BufWriter::new(file), Compression::None);
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
            estimate_blob_count(&mixed, &crate::host::Diagnostics::discard()).expect("estimate");
        assert!(!estimate.exact);
        let error = estimate.osmdata_blobs.abs_diff(actual_blobs);
        assert!(
            error * 100 < actual_blobs * 30,
            "estimated {} of {actual_blobs} actual blobs; relative error over 30 %",
            estimate.osmdata_blobs,
        );
    }
}
