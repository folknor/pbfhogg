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

use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use crate::blob::{parse_blob_header_with_index, BlobKind};
use crate::blob_meta::BlobIndex;
use crate::error::Result;

/// Size of the initial header probe `pread` per blob. One page covers the
/// 4-byte length prefix plus the header bytes for essentially every blob
/// in real PBFs; the fallback path in `next_header` handles the rare
/// exception where a blob's header exceeds the probe window.
const HEADER_PROBE_SIZE: usize = 4096;

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
    file: Arc<std::fs::File>,
    offset: u64,
    file_size: u64,
    header_buf: Vec<u8>,
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
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path).map_err(|e| {
            crate::error::new_error(crate::error::ErrorKind::Io(std::io::Error::other(
                format!("failed to open {}: {e}", path.display()),
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
            file: Arc::new(file),
            offset: 0,
            file_size,
            header_buf: Vec::new(),
        })
    }

    /// Shared `Arc<File>` for worker threads to `pread` data bodies.
    /// Used when the walker is handed off as a plan producer and the
    /// actual body reads happen on rayon workers (e.g. the diff
    /// parallel shard path, or the shared `scan::classify`
    /// schedule builders). Single-threaded callers ignore this.
    pub(crate) fn shared_file(&self) -> &Arc<std::fs::File> {
        &self.file
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
        match self.file.read_exact_at(&mut self.header_buf, self.offset) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(crate::error::new_error(crate::error::ErrorKind::Io(e))),
        }

        let header_len = u32::from_be_bytes([
            self.header_buf[0],
            self.header_buf[1],
            self.header_buf[2],
            self.header_buf[3],
        ]) as usize;
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

        let (blob_type, data_size, raw_index, tagdata) =
            parse_blob_header_with_index(&self.header_buf[4..header_end])?;
        let index = raw_index
            .as_ref()
            .and_then(|b| BlobIndex::deserialize(b));

        let data_offset = self.offset + header_end as u64;
        self.offset = data_offset + data_size as u64;
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

/// `pread` helper for consumers that already have the `Arc<File>`.
///
/// Equivalent to [`HeaderWalker::pread_data`] but without needing the
/// walker instance. Useful for worker threads that receive the shared
/// file and a list of `(data_offset, data_size)` descriptors.
#[allow(dead_code)]
pub(crate) fn pread_exact(
    file: &std::fs::File,
    offset: u64,
    size: usize,
    buf: &mut Vec<u8>,
) -> Result<()> {
    use std::os::unix::fs::FileExt as _;
    buf.resize(size, 0);
    file.read_exact_at(buf, offset)
        .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))
}

/// Read a blob's compressed data bytes through an arbitrary `Read`
/// source. Used when the blob is being streamed and we don't have
/// random access. For pread-based paths, use [`HeaderWalker::pread_data`].
#[allow(dead_code)]
pub(crate) fn read_blob_data<R: Read>(reader: &mut R, size: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; size];
    reader
        .read_exact(&mut buf)
        .map_err(|e| crate::error::new_error(crate::error::ErrorKind::Io(e)))?;
    Ok(buf)
}
