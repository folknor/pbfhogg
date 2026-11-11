//! Pread-only blob header walker.
//!
//! Walks a PBF file's blob headers by `pread`, skipping data bytes without
//! needing them: a probe of the length prefix and header per blob, or on
//! files of small frames a window of several frames per read
//! (`PreadSource`).
//! The file is opened with `posix_fadvise(RANDOM)` so the kernel does
//! not speculatively prefetch the blob bodies that the header walk
//! skips over; a walk honouring `--direct-io` opens it `O_DIRECT`
//! instead (`HeaderWalker::open_direct`) and reads aligned spans that
//! bypass the page cache. The one exception is a cold region of small
//! frames, where the walk reads windows and advises `WILLNEED` a bounded
//! distance ahead of them (`PreadSource`): there it is reading the bodies
//! anyway, and only explicit advice puts more than one read in flight.
//!
//! # When to use
//!
//! - Command paths that walk headers to build a per-blob plan and then
//!   selectively read a subset of blob bodies (e.g. `getid` include mode,
//!   the `scan::classify` schedule builders).
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
use crate::tunables::{
    HEADER_PROBE_SIZE, HEADER_WALK_PREFETCH_BYTES, HEADER_WINDOW_DENSE_FRAME_BYTES,
    HEADER_WINDOW_MISS_STREAK, HEADER_WINDOW_SAMPLE_FRAMES, HEADER_WINDOW_SIZE, SAMPLE_CAP,
};

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
    host: &crate::host::Entered<'_>,
) -> Result<BlobCountEstimate> {
    let source = input.into();
    let path = source.path();
    // A probe of the leading frames, not a read of the file: exempt from the
    // partial-index refusal, which the chosen arm's own read makes over the
    // whole file. Refusing here would judge only the sample, and would keep
    // the arm that does the work from ever meeting the file.
    let mut walker = HeaderWalker::open(source, host)?.accepting_partial_index();
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

/// How a walk reads its file by position: exactly `buf.len()` bytes at
/// `offset`, `UnexpectedEof` when the file ends first. A plain [`File`]
/// (buffered, through the page cache) or, with `linux-direct-io`, a
/// [`DirectPositional`] (`O_DIRECT`, aligned reads that bypass it).
///
/// [`File`]: std::fs::File
/// [`DirectPositional`]: crate::read::direct_reader::DirectPositional
pub(crate) trait Positional {
    fn pread_exact(&self, buf: &mut [u8], offset: u64) -> std::io::Result<()>;

    /// Read into `buf` at `offset` only what can be delivered without
    /// waiting on the device. Evidence about residency, never about the
    /// file: a miss says nothing about EOF or errors, which the exact read
    /// that follows decides.
    fn read_cached(&self, buf: &mut [u8], offset: u64) -> Cached;

    /// Advise that `len` (nonzero) bytes at `offset` will be read soon.
    /// Optional: whatever it does, the exact reads that follow decide every
    /// byte, EOF and error. Returns the bytes advised, a prefix from
    /// `offset` (0 when no advice was issued), as `fadvise::will_need`.
    fn will_need(&self, offset: u64, len: u64, invocation: &crate::host::Invocation) -> u64;
}

/// What [`Positional::read_cached`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cached {
    /// The first `n` (at least one) bytes were delivered.
    Hit(usize),
    /// Nothing could be delivered without waiting: not resident, in flight,
    /// contended, or a zero count (which inside the extent is not EOF).
    Miss,
    /// The transport cannot tell: no residency evidence will ever come.
    Unsupported,
}

impl Positional for std::fs::File {
    fn pread_exact(&self, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
        use std::os::unix::fs::FileExt as _;
        self.read_exact_at(buf, offset)
    }

    /// `preadv2(RWF_NOWAIT)`: a buffered read that returns `EAGAIN` rather
    /// than wait for the device.
    #[cfg(target_os = "linux")]
    fn read_cached(&self, buf: &mut [u8], offset: u64) -> Cached {
        use std::os::unix::io::AsRawFd as _;
        let Ok(offset) = libc::off_t::try_from(offset) else {
            return Cached::Miss;
        };
        let iov = libc::iovec {
            iov_base: buf.as_mut_ptr().cast(),
            iov_len: buf.len(),
        };
        // The crate's one `preadv2`.
        #[allow(clippy::disallowed_methods)]
        // Safety: the iovec covers `buf`, which the borrow keeps alive and
        // exclusive for the call, as the borrow of `self` keeps the
        // descriptor open.
        let n = unsafe {
            libc::preadv2(
                self.as_raw_fd(),
                &raw const iov,
                1,
                offset,
                libc::RWF_NOWAIT,
            )
        };
        match usize::try_from(n) {
            Ok(0) => Cached::Miss,
            Ok(n) => Cached::Hit(n.min(buf.len())),
            Err(_) => match std::io::Error::last_os_error().raw_os_error() {
                // No RWF_NOWAIT for buffered reads on this kernel or
                // filesystem, or no preadv2 at all.
                Some(libc::EOPNOTSUPP | libc::ENOSYS | libc::EINVAL) => Cached::Unsupported,
                _ => Cached::Miss,
            },
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn read_cached(&self, _buf: &mut [u8], _offset: u64) -> Cached {
        Cached::Unsupported
    }

    fn will_need(&self, offset: u64, len: u64, invocation: &crate::host::Invocation) -> u64 {
        crate::fadvise::will_need(self, offset, len, invocation)
    }
}

#[cfg(feature = "linux-direct-io")]
impl Positional for crate::read::direct_reader::DirectPositional {
    fn pread_exact(&self, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
        self.read_exact_at(buf, offset)
    }

    /// An `O_DIRECT` read never comes from the page cache: every read waits
    /// on the device, so every refill is a miss, without a syscall.
    fn read_cached(&self, _buf: &mut [u8], _offset: u64) -> Cached {
        Cached::Miss
    }

    /// No advice: `O_DIRECT` reads never come from the page cache.
    fn will_need(&self, _offset: u64, _len: u64, _invocation: &crate::host::Invocation) -> u64 {
        0
    }
}

/// The walker's byte source: `pread` at the walk's position on the shared
/// file, bounded by the size captured at open.
///
/// Reads come in windows that persist across payload skips: a frame whose
/// prefix and header lie inside the current window costs no syscall. When the
/// walk reaches a position outside it, a refill reads at that position
/// ([`Self::refill`]): a [`HEADER_PROBE_SIZE`] probe covering the length
/// prefix and (in the common case) the whole header, or, on a cold dense
/// file, a [`HEADER_WINDOW_SIZE`] window covering many frames.
///
/// Why windows: the walk is a dependency chain - frame N+1's offset comes
/// from frame N's header - so no number of in-flight exact probes could
/// overlap it, and on a cold cache each probe is one device latency with
/// nothing else in flight. Reading ahead is the only way to resolve more than
/// one header per latency. Two gates keep it from reading payload bytes for
/// nothing:
///
/// - Density: only while the mean of the last [`HEADER_WINDOW_SAMPLE_FRAMES`]
///   completed frames is below [`HEADER_WINDOW_DENSE_FRAME_BYTES`]. On sparse
///   files (the planet dump) a window holds about one header. The first
///   completed frame (the OSMHeader) does not count, and until one data
///   frame has completed the walk probes.
/// - Residency: a dense refill first tries a cache-only read
///   ([`Positional::read_cached`]); a hit is the window, at no I/O. Only
///   after [`HEADER_WINDOW_MISS_STREAK`] dense refills in a row have missed
///   does an exact refill read a window rather than a probe. Without this a
///   walk over headers an earlier walk left cached, bodies not, read the
///   bodies: europe `check --refs` measured 0.27 s and no disk read for its
///   schedule walk with probes, 19.8 s and 15.9 GB with density-gated windows
///   alone (`reference/performance/read-pipeline.md`). A transport that cannot tell
///   residency never reads windows; the `O_DIRECT` one always misses, since
///   none of its reads come from cache.
///
/// Prefetch: a window is still one synchronous read (under `RANDOM`, Linux
/// fills it in readahead-sized steps one after another), so windows alone
/// walk a cold file at queue depth one. A window read at or past the end of
/// the last prefetch episode opens the next: `WILLNEED` over the
/// [`HEADER_WALK_PREFETCH_BYTES`] after the window, clipped to the captured
/// extent, which `fadvise::will_need` issues in calls the kernel honours
/// whole (it drops what a longer call asks past its cap). Renewal needs fresh evidence: when the
/// walk crosses the end of an episode the miss streak restarts, so misses on
/// pages the walk's own advice put in flight never open the next episode,
/// and a region whose headers are cached (hits) ends the prefetch within one
/// episode. Advice is optional: the exact reads decide every byte, EOF and
/// error as before, and the `O_DIRECT` transport gives none.
///
/// A header past the window costs one more `pread` for exactly its
/// remainder. A payload is never read on purpose: skipping it is a check of
/// its extent against the captured size.
///
/// The frame size is measured here, from positions: `fill` is only ever
/// asked for a length prefix (`FrameSource::fill`), so it marks a frame's
/// start, and the payload skip that follows marks its end.
///
/// The extent is captured at open, and bytes already read into the window are
/// retained: a file that shrank after open fails the next `pread` inside the
/// captured extent (`UnexpectedEof`, an error) rather than reading as a
/// shorter file, but headers a window already holds are served from it. Only
/// a captured remainder of 0-3 bytes at a frame boundary is the clean tail,
/// and it is recognised without reading it, window or not.
///
/// The extent policy lives here for every transport `P`: a transport only
/// reads exactly the bytes it is asked for, so the direct one's aligned
/// padding can neither widen the probe past the captured size nor turn a
/// shrunk file into a clean end.
pub(crate) struct PreadSource<P = std::fs::File> {
    file: Arc<P>,
    pos: u64,
    file_size: u64,
    /// The current frame's probe: `window_len` bytes of the file read at
    /// `window_at`.
    window: Vec<u8>,
    window_at: u64,
    window_len: usize,
    /// Where the frame being walked started (set by the prefix `fill`).
    frame_start: u64,
    /// Completed frames so far, the OSMHeader included.
    frames_done: u64,
    /// Sizes of the last [`HEADER_WINDOW_SAMPLE_FRAMES`] completed data
    /// frames, a ring indexed by the data-frame count.
    recent: [u64; HEADER_WINDOW_SAMPLE_FRAMES],
    /// Consecutive dense refills whose cache-only read missed.
    miss_streak: u32,
    /// Whether the transport can tell residency; once it says it cannot,
    /// no window is ever read.
    residency_known: bool,
    /// End of the last prefetch episode: advice was attempted (issued or
    /// not, honoured or not) for bytes up to here. Zero before the first.
    advised_to: u64,
    /// Whether the walk is inside an episode it has not yet crossed the end
    /// of.
    in_episode: bool,
    /// The invocation advice failures and wait time count into.
    invocation: crate::host::Invocation,
    stats: WindowStats,
}

/// What a walk's refills did, emitted as counters when the walk ends.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct WindowStats {
    /// Dense refills served by a cache-only read.
    pub(crate) cached_hits: u64,
    /// Dense refills whose cache-only read could not deliver.
    pub(crate) cached_misses: u64,
    /// Cache-only reads the transport refused (at most one per walk).
    pub(crate) cached_unsupported: u64,
    /// Windows read, and the bytes they asked for.
    pub(crate) windows: u64,
    pub(crate) window_bytes: u64,
    /// Prefetch episodes opened, and the bytes their `WILLNEED` calls asked
    /// for (requested, not read: the kernel may ignore advice).
    pub(crate) prefetch_episodes: u64,
    pub(crate) prefetch_bytes: u64,
}

impl<P> PreadSource<P> {
    fn new(file: Arc<P>, file_size: u64, invocation: crate::host::Invocation) -> Self {
        Self {
            file,
            pos: 0,
            file_size,
            window: Vec::new(),
            window_at: 0,
            window_len: 0,
            frame_start: 0,
            frames_done: 0,
            recent: [0; HEADER_WINDOW_SAMPLE_FRAMES],
            miss_streak: 0,
            residency_known: true,
            advised_to: 0,
            in_episode: false,
            invocation,
            stats: WindowStats::default(),
        }
    }

    /// Record a completed frame ending at the current position.
    fn complete_frame(&mut self) {
        if self.frames_done > 0 {
            // A data frame: frames after the first. The slot is below the
            // ring length, a usize.
            #[allow(clippy::cast_possible_truncation)]
            let slot = ((self.frames_done - 1) % HEADER_WINDOW_SAMPLE_FRAMES as u64) as usize;
            self.recent[slot] = self.pos.saturating_sub(self.frame_start);
        }
        self.frames_done += 1;
    }

    /// Whether recent frames are dense: false before any data frame has
    /// completed.
    fn dense(&self) -> bool {
        let samples = self
            .frames_done
            .saturating_sub(1)
            .min(HEADER_WINDOW_SAMPLE_FRAMES as u64);
        if samples == 0 {
            return false;
        }
        #[allow(clippy::cast_possible_truncation)]
        let total: u64 = self.recent[..samples as usize].iter().sum();
        total / samples < HEADER_WINDOW_DENSE_FRAME_BYTES
    }

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

impl<P: Positional> PreadSource<P> {
    /// Refill the window at the walk's position, which lies inside the
    /// captured extent. Sparse: one exact probe. Dense: first a cache-only
    /// probe, whose hit (any positive count) is the window; on a miss, an
    /// exact read, the [`HEADER_WINDOW_SIZE`] window once
    /// [`HEADER_WINDOW_MISS_STREAK`] dense refills in a row have missed,
    /// else the probe.
    fn refill(&mut self) -> std::io::Result<()> {
        let remaining = usize::try_from(self.remaining()).unwrap_or(usize::MAX);
        let mut size = remaining.min(HEADER_PROBE_SIZE);
        if self.in_episode && self.pos >= self.advised_to {
            // Past the episode: only misses in unadvised bytes count now.
            self.in_episode = false;
            self.miss_streak = 0;
        }
        if !self.dense() {
            self.miss_streak = 0;
        } else if self.residency_known {
            self.window.resize(size, 0);
            match self.file.read_cached(&mut self.window, self.pos) {
                Cached::Hit(n) => {
                    self.stats.cached_hits += 1;
                    self.miss_streak = 0;
                    self.window_at = self.pos;
                    self.window_len = n;
                    return Ok(());
                }
                Cached::Miss => {
                    self.stats.cached_misses += 1;
                    self.miss_streak = self.miss_streak.saturating_add(1);
                }
                Cached::Unsupported => {
                    self.stats.cached_unsupported += 1;
                    self.residency_known = false;
                    self.miss_streak = 0;
                }
            }
            if self.residency_known && self.miss_streak >= HEADER_WINDOW_MISS_STREAK {
                size = remaining.min(HEADER_WINDOW_SIZE);
                self.stats.windows += 1;
                self.stats.window_bytes += size as u64;
                if !self.in_episode {
                    // Advised before the window's own read, so the device
                    // works on the episode while the walk waits on it.
                    self.open_episode(self.pos + size as u64);
                }
            }
        }
        self.window.resize(size, 0);
        self.file.pread_exact(&mut self.window, self.pos)?;
        self.window_at = self.pos;
        self.window_len = size;
        Ok(())
    }

    /// Advise `WILLNEED` over the episode from `start` (the end of the
    /// window being read), clipped to the captured extent.
    fn open_episode(&mut self, start: u64) {
        let end = start
            .saturating_add(HEADER_WALK_PREFETCH_BYTES)
            .min(self.file_size);
        if end <= start {
            return;
        }
        // A transport (or platform) that gives no advice advises 0 bytes: no
        // episode, so the walk's window policy stays exactly what it was.
        let issued = self.file.will_need(start, end - start, &self.invocation);
        if issued > 0 {
            self.in_episode = true;
            self.advised_to = start + issued;
            self.stats.prefetch_episodes += 1;
            self.stats.prefetch_bytes += issued;
        }
    }
}

impl<P: Positional> crate::frame::FrameSource for PreadSource<P> {
    fn fill(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // A frame boundary: `fill` is only asked for a length prefix.
        self.frame_start = self.pos;
        if self.remaining() < buf.len() as u64 {
            // The clean tail (or the end): not read, as the walk always did,
            // whether or not a window holds it.
            return Ok(0);
        }
        let mut filled = 0;
        while filled < buf.len() {
            if self.windowed().is_empty() {
                // The remaining extent covers the whole request, so a prefix
                // the old window ended inside is completed here, never cut
                // short.
                self.refill()?;
            }
            let have = self.windowed();
            let n = have.len().min(buf.len() - filled);
            buf[filled..filled + n].copy_from_slice(&have[..n]);
            self.pos += n as u64;
            filled += n;
        }
        Ok(filled)
    }

    fn append(&mut self, n: usize, out: &mut Vec<u8>) -> std::io::Result<usize> {
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
            if let Err(e) = self.file.pread_exact(&mut out[start..], self.pos) {
                out.truncate(start);
                return Err(e);
            }
            self.pos += rest as u64;
        }
        Ok(take)
    }
}

impl<P: Positional> crate::frame::FrameSkip for PreadSource<P> {
    fn skip_bytes(&mut self, n: u64) -> std::io::Result<bool> {
        // The window is kept: the next prefix may lie inside it.
        if n > self.remaining() {
            return Ok(false);
        }
        self.pos += n;
        self.complete_frame();
        Ok(true)
    }
}

/// Walks blob headers via `pread`. Call [`Self::next_header`] repeatedly
/// until it returns `None`. Use [`Self::pread_data`] to fetch a blob's
/// data payload on demand.
///
/// `P` is how the file is read ([`Positional`]): buffered by default
/// ([`Self::open`]), or `O_DIRECT` ([`Self::open_direct`]). Only a buffered
/// walker hands its file to other threads ([`Self::input`],
/// [`Self::shared_file`]): the pools that take it pread unaligned ranges,
/// which an `O_DIRECT` descriptor refuses, so a direct walker offering its
/// descriptor is a compile error rather than a run-time `EINVAL`.
pub(crate) struct HeaderWalker<P = std::fs::File> {
    /// The frame walk over the file. It owns the frame policy and locates
    /// every error it raises at the frame, naming this walker's file: a byte
    /// offset on a multi-input command does not say which input it indexes,
    /// and this walker is the preflight that most multi-input commands fail
    /// in first. A tripped cancellation token ends the walk with
    /// `ErrorKind::Cancelled`, unlocated.
    frames: Frames<PreadSource<P>>,
    diagnostics: crate::host::Diagnostics,
    reported_unindexed_filter: bool,
    file: Arc<P>,
    /// The file this walker was opened from.
    path: Arc<Path>,
    file_size: u64,
}

impl HeaderWalker {
    /// Open `source` buffered (a new open file description of it, so the
    /// advice below stays this walker's) and hint `posix_fadvise(RANDOM)` to
    /// the kernel.
    /// Errors if the file cannot be opened or metadata read.
    ///
    /// Buffered probes and payload reads go through the page cache, so the
    /// headers a walk read stay cached for a later walk of the same file
    /// (a warm walk is far cheaper than a cold one; reference/performance/read-pipeline.md
    /// records the swing). A caller honouring `--direct-io` opens the walk
    /// with [`Self::open_direct`] instead, giving that residency up.
    pub(crate) fn open<'a>(
        source: impl Into<crate::read::input::InputSource<'a>>,
        host: &crate::host::Entered<'_>,
    ) -> Result<Self> {
        let source = source.into();
        let path = source.path();
        // RANDOM (`InputSource::open_random_access` says why, and which
        // pools that take this description over read under it).
        let file = source.open_random_access(&host.invocation)?;
        let file_size = stat_size(&file, path)?;
        Ok(Self::over(Arc::new(file), file_size, path, host))
    }

    /// The walker's file handle and path, for the pools that pread the blobs
    /// this walk scheduled and locate their failures in this input.
    pub(crate) fn input(&self) -> crate::read::input::InputFile {
        crate::read::input::InputFile::from_parts(Arc::clone(&self.file), Arc::clone(&self.path))
    }

    /// Shared `Arc<File>` for worker threads to `pread` data bodies.
    /// Used when the walker is handed off as a plan producer and the
    /// actual body reads happen on worker threads (e.g. altw sparse's
    /// passthrough pool). Single-threaded callers ignore this.
    pub(crate) fn shared_file(&self) -> &Arc<std::fs::File> {
        &self.file
    }
}

#[cfg(feature = "linux-direct-io")]
impl HeaderWalker<crate::read::direct_reader::DirectPositional> {
    /// Open `source` with `O_DIRECT`: every header probe and payload read
    /// is an aligned read of the enclosing aligned span
    /// (`direct_reader::read_aligned`), so the walk does not fill the page
    /// cache, nor find in it what an earlier walk read. A probe is one read
    /// of the aligned blocks covering its window, which a buffered probe
    /// could instead find already cached. The usual caveat holds: a
    /// filesystem may serve an `O_DIRECT` read through its cache anyway
    /// (`crate::direct_io`).
    ///
    /// A filesystem that refuses `O_DIRECT`, or an alignment the buffers
    /// cannot keep, is the `direct-io` platform refusal, and a replaced
    /// pinned input its own refusal; any other open failure names the
    /// input, as `FileReader` names it. No advice is given: there is no
    /// readahead to suppress. The walker cannot be moved to another thread
    /// (its reads share one aligned scratch buffer).
    pub(crate) fn open_direct<'a>(
        source: impl Into<crate::read::input::InputSource<'a>>,
        host: &crate::host::Entered<'_>,
    ) -> Result<Self> {
        let source = source.into();
        let path = source.path();
        let (file, align) = source.open_direct(Some(&host.invocation)).map_err(|e| {
            if matches!(
                e.kind(),
                crate::ErrorKind::PlatformUnsupported { .. } | crate::ErrorKind::InputReplaced
            ) {
                e
            } else {
                e.context(format!("failed to open {}", path.display()))
            }
        })?;
        let file_size = stat_size(&file, path)?;
        #[cfg(test)]
        tests::record_direct_open();
        #[expect(
            clippy::arc_with_non_send_sync,
            reason = "shared only between this walker and its own frame source, on the \
                      thread that opened it; the walker generic over Arc<P> keeps one \
                      shape for the buffered walker, whose Arc does cross threads"
        )]
        let file = Arc::new(crate::read::direct_reader::DirectPositional::new(
            file,
            align,
            &host.invocation,
        ));
        Ok(Self::over(file, file_size, path, host))
    }
}

/// The size of `file`, captured once: the one extent the walk checks
/// against, whatever the transport.
fn stat_size(file: &std::fs::File, path: &Path) -> Result<u64> {
    Ok(file
        .metadata()
        .map_err(|e| crate::Error::from(e).context(format!("failed to stat {}", path.display())))?
        .len())
}

impl<P: Positional> HeaderWalker<P> {
    /// A walk from the start of `file`, whose size was captured as
    /// `file_size`.
    fn over(file: Arc<P>, file_size: u64, path: &Path, host: &crate::host::Entered<'_>) -> Self {
        let source = PreadSource::new(
            Arc::clone(&file),
            file_size,
            host.invocation.handle().clone(),
        );
        Self {
            frames: Frames::of_input(source, path, host),
            diagnostics: host.diagnostics.clone(),
            reported_unindexed_filter: false,
            file,
            path: Arc::from(path),
            file_size,
        }
    }

    /// Exempt this walk from the command-input refusal of a partially
    /// indexed file (`Frames::refuse_partial_index`), which every walk has
    /// by default: without this, [`Self::next_header`] ends the walk with
    /// the typed missing-indexdata error, located at the first data blob
    /// without readable indexdata, once both kinds have been seen. Only for
    /// `cat` (the remedy), `inspect`'s descriptions and the blob-count
    /// probe, whose caller's chosen arm judges the file; the call site says
    /// which.
    #[must_use]
    pub(crate) fn accepting_partial_index(mut self) -> Self {
        self.frames.accept_partial_index();
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
        // Once per walk, nested under the command that runs the walk, so
        // never refused there; a sealed invocation reports nothing.
        let Ok(call) = self.frames.cancel_token().call() else {
            return;
        };
        crate::host::diag_once!(
            self.diagnostics,
            &call,
            Note,
            "reader.filter_without_indexdata",
            "Input contains blobs without indexdata; blob type and spatial filters cannot skip those blobs"
        );
        call.counter("schedule_filter_without_indexdata", 1);
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
    /// Serves the 4-byte length prefix and the blob header from the source's
    /// current window, or refills it with one `pread` (a [`HEADER_PROBE_SIZE`]
    /// probe, or a [`HEADER_WINDOW_SIZE`] window on dense files; see
    /// [`PreadSource`]). A second `pread` is only needed for a header that
    /// extends past the window. The payload is not read: the walk
    /// refuses a frame whose declared payload runs past the file, located at
    /// that frame, so a truncated tail blob cannot end the walk silently.
    pub(crate) fn next_header(&mut self) -> Result<Option<BlobHeaderMeta>> {
        let Some(header) = self.frames.next_header()? else {
            self.report_windows();
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
        Ok(Some(meta))
    }

    /// What this walk's refills did so far.
    pub(crate) fn window_stats(&self) -> WindowStats {
        self.frames.src.stats
    }

    /// Emit the refill counters of a walk that reached its end. Nested under
    /// the command running the walk, so never refused there; a sealed
    /// invocation reports nothing.
    fn report_windows(&self) {
        let Ok(call) = self.frames.cancel_token().call() else {
            return;
        };
        let stats = self.window_stats();
        call.counter("header_walk_cached_hits", stats.cached_hits);
        call.counter("header_walk_cached_misses", stats.cached_misses);
        call.counter("header_walk_cached_unsupported", stats.cached_unsupported);
        call.counter("header_walk_windows", stats.windows);
        call.counter("header_walk_window_bytes", stats.window_bytes);
        call.counter("header_walk_prefetch_episodes", stats.prefetch_episodes);
        call.counter("header_walk_prefetch_bytes", stats.prefetch_bytes);
    }

    /// `pread` `size` bytes at `offset` (a payload or a whole frame) into
    /// `buf` and return them. A caller keeping `buf` across blobs applies its
    /// retention limit (`ReadBuf::release_outlier`) once it is done with them.
    pub(crate) fn pread_data<'b>(
        &self,
        offset: u64,
        size: usize,
        buf: &'b mut crate::read::read_buf::ReadBuf,
    ) -> Result<&'b [u8]> {
        Ok(buf.read_exact_at(&*self.file, offset, size)?)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{BlobCountEstimate, SAMPLE_CAP, ScanArm, choose_scan_arm_at, estimate_blob_count};

    #[cfg(feature = "linux-direct-io")]
    thread_local! {
        /// `O_DIRECT` walks opened on this thread. Thread-local so tests
        /// running concurrently cannot see each other's opens.
        static DIRECT_OPENS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    #[cfg(feature = "linux-direct-io")]
    pub(super) fn record_direct_open() {
        DIRECT_OPENS.with(|n| n.set(n.get() + 1));
    }

    /// How many `O_DIRECT` walks this thread has opened so far.
    #[cfg(feature = "linux-direct-io")]
    pub(crate) fn direct_opens() -> usize {
        DIRECT_OPENS.with(std::cell::Cell::get)
    }

    /// Walk the whole file, returning the frame count or the first error.
    fn count_frames<P: super::Positional>(
        mut walker: super::HeaderWalker<P>,
    ) -> super::Result<usize> {
        let mut n = 0;
        while walker.next_header()?.is_some() {
            n += 1;
        }
        Ok(n)
    }

    /// An in-memory file that logs every exact positional read as
    /// `(offset, len)`, can be cut short mid-walk, and answers cache-only
    /// reads from its `resident` ranges (none: a cold file), or refuses them
    /// all when `no_residency`.
    #[derive(Default)]
    struct LoggedFile {
        bytes: std::sync::Mutex<Vec<u8>>,
        reads: std::sync::Mutex<Vec<(u64, usize)>>,
        resident: Vec<std::ops::Range<u64>>,
        no_residency: bool,
        /// Every `WILLNEED` asked for, as `(offset, len)`.
        advised: std::sync::Mutex<Vec<(u64, u64)>>,
        /// Whether advised bytes become resident (advice that landed).
        advice_lands: bool,
    }

    impl LoggedFile {
        fn new(bytes: Vec<u8>) -> Self {
            Self {
                bytes: std::sync::Mutex::new(bytes),
                ..Self::default()
            }
        }
    }

    impl super::Positional for LoggedFile {
        #[allow(clippy::unwrap_in_result)]
        fn pread_exact(&self, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
            self.reads.lock().expect("reads").push((offset, buf.len()));
            let bytes = self.bytes.lock().expect("bytes");
            let start = usize::try_from(offset).expect("offset");
            let Some(src) = bytes.get(start..start + buf.len()) else {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            };
            buf.copy_from_slice(src);
            Ok(())
        }

        fn read_cached(&self, buf: &mut [u8], offset: u64) -> super::Cached {
            if self.no_residency {
                return super::Cached::Unsupported;
            }
            let landed: Vec<std::ops::Range<u64>> = if self.advice_lands {
                let advised = self.advised.lock().expect("advised");
                advised.iter().map(|&(at, len)| at..at + len).collect()
            } else {
                Vec::new()
            };
            let Some(range) = self
                .resident
                .iter()
                .chain(landed.iter())
                .find(|r| r.contains(&offset))
                .cloned()
            else {
                return super::Cached::Miss;
            };
            let n = buf
                .len()
                .min(usize::try_from(range.end - offset).expect("range"));
            let bytes = self.bytes.lock().expect("bytes");
            let start = usize::try_from(offset).expect("offset");
            buf[..n].copy_from_slice(&bytes[start..start + n]);
            super::Cached::Hit(n)
        }

        fn will_need(&self, offset: u64, len: u64, _invocation: &crate::host::Invocation) -> u64 {
            self.advised.lock().expect("advised").push((offset, len));
            len
        }
    }

    #[allow(clippy::cast_possible_truncation)]
    fn varint(mut v: u64, out: &mut Vec<u8>) {
        while v >= 0x80 {
            out.push((v as u8) | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
    }

    /// One raw frame: prefix, a BlobHeader of `kind` declaring `payload`
    /// bytes plus `pad` bytes of an unknown header field, then the payload.
    fn raw_frame(kind: &str, payload: usize, pad: usize) -> Vec<u8> {
        let mut header = vec![0x0A];
        varint(kind.len() as u64, &mut header);
        header.extend_from_slice(kind.as_bytes());
        header.push(0x18);
        varint(payload as u64, &mut header);
        if pad > 0 {
            // Field 15, length-delimited: skipped as unknown.
            header.push(0x7A);
            varint(pad as u64, &mut header);
            header.extend(std::iter::repeat_n(0x55, pad));
        }
        let mut frame = u32::try_from(header.len())
            .expect("len")
            .to_be_bytes()
            .to_vec();
        frame.extend_from_slice(&header);
        frame.extend(std::iter::repeat_n(0xAB, payload));
        frame
    }

    /// The size of a raw data frame carrying `payload` bytes and no pad.
    fn frame_len(payload: usize) -> usize {
        raw_frame("OSMData", payload, 0).len()
    }

    /// A file of an OSMHeader frame then one OSMData frame per `(payload,
    /// pad)`, with the frame starts.
    fn raw_file(frames: &[(usize, usize)]) -> (Vec<u8>, Vec<u64>) {
        let mut bytes = raw_frame("OSMHeader", 16, 0);
        let mut starts = vec![0];
        for &(payload, pad) in frames {
            starts.push(bytes.len() as u64);
            bytes.extend(raw_frame("OSMData", payload, pad));
        }
        (bytes, starts)
    }

    /// Walk a cold `bytes` through the pread source, checking every frame
    /// start against `starts`, and return the exact reads it made.
    fn walk_logged(bytes: Vec<u8>, starts: &[u64]) -> Vec<(u64, usize)> {
        walk_file(LoggedFile::new(bytes), starts).0
    }

    /// [`walk_logged`] over a prepared file, also returning the walk's
    /// refill statistics.
    fn walk_file(file: LoggedFile, starts: &[u64]) -> (Vec<(u64, usize)>, super::WindowStats) {
        let file = Arc::new(file);
        let stats = walk_shared(&file, starts);
        let reads = file.reads.lock().expect("reads").clone();
        (reads, stats)
    }

    /// [`walk_file`] over a file the caller keeps, for its advice log.
    fn walk_shared(file: &Arc<LoggedFile>, starts: &[u64]) -> super::WindowStats {
        let host_owner = crate::host::Host::for_tests();
        let host = host_owner.entered();
        let size = file.bytes.lock().expect("bytes").len() as u64;
        let mut walker = super::HeaderWalker::over(
            Arc::clone(file),
            size,
            std::path::Path::new("logged.pbf"),
            &host,
        )
        .accepting_partial_index();
        let mut seen = Vec::new();
        while let Some(meta) = walker.next_header().expect("walk") {
            seen.push(meta.frame_start);
        }
        assert_eq!(seen, starts, "the walk's frame starts");
        walker.window_stats()
    }

    use crate::tunables::{
        HEADER_PROBE_SIZE, HEADER_WALK_PREFETCH_BYTES, HEADER_WINDOW_DENSE_FRAME_BYTES,
        HEADER_WINDOW_MISS_STREAK, HEADER_WINDOW_SAMPLE_FRAMES, HEADER_WINDOW_SIZE,
    };
    use std::sync::Arc;

    /// A cold dense file of `frames` 60 KB frames, enough to span more than
    /// two prefetch episodes.
    fn prefetch_file() -> (Vec<u8>, Vec<u64>) {
        let frames = usize::try_from(3 * HEADER_WALK_PREFETCH_BYTES / 60_000).expect("frames");
        raw_file(&vec![(60_000, 0); frames])
    }

    /// The advice a walk gave, as episodes: maximal runs of contiguous calls.
    fn episodes(advised: &[(u64, u64)]) -> Vec<std::ops::Range<u64>> {
        let mut out: Vec<std::ops::Range<u64>> = Vec::new();
        for &(at, len) in advised {
            match out.last_mut() {
                Some(last) if last.end == at => last.end = at + len,
                _ => out.push(at..at + len),
            }
        }
        out
    }

    /// A cold dense walk advises ahead of its first window, one request per
    /// episode within the extent (the split into calls the kernel honours is
    /// `fadvise::will_need`'s, tested there), each byte once, and opens
    /// each later episode only from a window past the previous one's end:
    /// with advice that never lands, every episode is reached cold and renews.
    #[test]
    fn a_cold_dense_walk_prefetches_in_bounded_renewed_episodes() {
        let (bytes, starts) = prefetch_file();
        let size = bytes.len() as u64;
        let file = Arc::new(LoggedFile::new(bytes));
        let stats = walk_shared(&file, &starts);
        let advised = file.advised.lock().expect("advised").clone();
        assert!(
            advised
                .iter()
                .all(|&(at, len)| len > 0 && len <= HEADER_WALK_PREFETCH_BYTES && at + len <= size),
            "{advised:?}"
        );
        let eps = episodes(&advised);
        assert!(eps.len() >= 3, "{eps:?}");
        assert_eq!(eps.len(), advised.len(), "one request per episode");
        assert_eq!(stats.prefetch_episodes, eps.len() as u64);
        assert_eq!(
            stats.prefetch_bytes,
            advised.iter().map(|&(_, len)| len).sum::<u64>()
        );
        // The first starts where the first window ends.
        let first_window = starts[FIRST_WINDOW_READ + 1];
        assert_eq!(eps[0].start, first_window + HEADER_WINDOW_SIZE as u64);
        for pair in eps.windows(2) {
            assert!(
                pair[1].start > pair[0].end,
                "renewed inside its own episode: {eps:?}"
            );
        }
        for ep in &eps[..eps.len() - 1] {
            assert_eq!(ep.end - ep.start, HEADER_WALK_PREFETCH_BYTES, "{eps:?}");
        }
    }

    /// Advice that lands turns the episode's headers into hits, and a tail
    /// whose headers are cached (bodies cold) gives no fresh misses: the
    /// prefetch ends after the one episode the cold prefix opened.
    #[test]
    fn a_cached_header_tail_ends_the_prefetch_within_one_episode() {
        let (bytes, starts) = prefetch_file();
        let mut file = LoggedFile::new(bytes);
        file.advice_lands = true;
        let tail_from = HEADER_WALK_PREFETCH_BYTES;
        file.resident = starts
            .iter()
            .filter(|&&s| s >= tail_from)
            .map(|&s| s..s + 64)
            .collect();
        let file = Arc::new(file);
        let stats = walk_shared(&file, &starts);
        assert_eq!(stats.prefetch_episodes, 1, "{stats:?}");
        assert!(
            stats.prefetch_bytes <= HEADER_WALK_PREFETCH_BYTES,
            "{stats:?}"
        );
    }

    /// A transport that gives no advice opens no episode, so its windows
    /// are exactly what they were without prefetch.
    #[test]
    fn a_transport_without_advice_opens_no_episode() {
        struct NoAdvice(LoggedFile);
        impl super::Positional for NoAdvice {
            fn pread_exact(&self, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
                self.0.pread_exact(buf, offset)
            }
            fn read_cached(&self, buf: &mut [u8], offset: u64) -> super::Cached {
                self.0.read_cached(buf, offset)
            }
            fn will_need(&self, _: u64, _: u64, _: &crate::host::Invocation) -> u64 {
                0
            }
        }
        let (bytes, starts) = prefetch_file();
        let size = bytes.len() as u64;
        let host_owner = crate::host::Host::for_tests();
        let host = host_owner.entered();
        let mut walker = super::HeaderWalker::over(
            Arc::new(NoAdvice(LoggedFile::new(bytes))),
            size,
            std::path::Path::new("logged.pbf"),
            &host,
        )
        .accepting_partial_index();
        let mut seen = Vec::new();
        while let Some(meta) = walker.next_header().expect("walk") {
            seen.push(meta.frame_start);
        }
        assert_eq!(seen, starts);
        let stats = walker.window_stats();
        assert_eq!((stats.prefetch_episodes, stats.prefetch_bytes), (0, 0));
        // One miss streak at the start, then windows to the end.
        assert_eq!(
            stats.cached_misses,
            stats.windows + u64::from(HEADER_WINDOW_MISS_STREAK) - 1
        );
    }

    /// The refill index (counting exact reads) of a cold dense walk's first
    /// window: the OSMHeader's probe, then one probe per miss of the streak.
    const FIRST_WINDOW_READ: usize = HEADER_WINDOW_MISS_STREAK as usize;

    /// A cold dense file reads windows once a data frame has completed and
    /// the miss streak has run, so far fewer reads than frames.
    #[test]
    fn a_cold_dense_walk_reads_windows_after_its_miss_streak() {
        let (bytes, starts) = raw_file(&[(10_000, 0); 300]);
        let reads = walk_logged(bytes, &starts);
        assert!(
            reads[..FIRST_WINDOW_READ]
                .iter()
                .all(|&(_, len)| len <= HEADER_PROBE_SIZE),
            "{reads:?}"
        );
        assert_eq!(
            reads[FIRST_WINDOW_READ],
            (starts[FIRST_WINDOW_READ + 1], HEADER_WINDOW_SIZE),
            "{reads:?}"
        );
        assert!(reads.len() < 20, "{} reads for 301 frames", reads.len());
    }

    /// Headers an earlier walk left cached, bodies not: every dense refill
    /// is a cache hit and no window reads a body.
    #[test]
    fn cached_headers_over_cold_bodies_read_no_window() {
        let (bytes, starts) = raw_file(&[(10_000, 0); 300]);
        let mut file = LoggedFile::new(bytes);
        file.resident = starts.iter().map(|&s| s..s + 64).collect();
        let (reads, stats) = walk_file(file, &starts);
        assert_eq!(stats.windows, 0, "{stats:?}");
        assert_eq!(stats.cached_misses, 0, "{stats:?}");
        assert!(
            reads.iter().all(|&(_, len)| len <= HEADER_PROBE_SIZE),
            "{reads:?}"
        );
    }

    /// Cached headers with scattered cold ones (every third): the misses
    /// never run to the streak, so no window is read.
    #[test]
    fn scattered_cold_headers_read_no_window() {
        let (bytes, starts) = raw_file(&[(10_000, 0); 300]);
        let mut file = LoggedFile::new(bytes);
        file.resident = starts
            .iter()
            .enumerate()
            .filter(|(i, _)| i % 3 != 0)
            .map(|(_, &s)| s..s + 64)
            .collect();
        let (_, stats) = walk_file(file, &starts);
        assert_eq!(stats.windows, 0, "{stats:?}");
        assert!(stats.cached_misses > 0, "{stats:?}");
    }

    /// A transport that cannot tell residency reads no windows, and is asked
    /// once.
    #[test]
    fn unknowable_residency_reads_no_window() {
        let (bytes, starts) = raw_file(&[(10_000, 0); 300]);
        let mut file = LoggedFile::new(bytes);
        file.no_residency = true;
        let (_, stats) = walk_file(file, &starts);
        assert_eq!(stats.windows, 0, "{stats:?}");
        assert_eq!(stats.cached_unsupported, 1, "{stats:?}");
    }

    /// The real buffered transport: a page just written is resident, so a
    /// cache-only read of it delivers (or the platform cannot tell).
    #[test]
    fn a_cache_only_read_of_a_fresh_file_delivers() {
        use super::Positional as _;
        let dir = crate::path_guard::tests::scratch();
        let path = dir.path().join("fresh.bin");
        std::fs::write(&path, [7_u8; 8192]).expect("write");
        let file = std::fs::File::open(&path).expect("open");
        let mut buf = [0_u8; 4096];
        match file.read_cached(&mut buf, 100) {
            super::Cached::Hit(n) => {
                assert!(n > 0 && n <= buf.len());
                assert!(buf[..n].iter().all(|&b| b == 7));
            }
            super::Cached::Unsupported => {}
            super::Cached::Miss => panic!("a page just written read as not resident"),
        }
    }

    /// Small and large frames alternating read as their mean: no window.
    #[test]
    fn alternating_frame_sizes_keep_the_probe() {
        let frames: Vec<_> = (0..40)
            .map(|i| {
                if i % 2 == 0 {
                    (16 * 1024, 0)
                } else {
                    (2 * 1024 * 1024, 0)
                }
            })
            .collect();
        let (bytes, starts) = raw_file(&frames);
        let reads = walk_logged(bytes, &starts);
        let windows = reads
            .iter()
            .filter(|&&(_, len)| len > HEADER_PROBE_SIZE)
            .count();
        assert_eq!(windows, 0, "{reads:?}");
    }

    /// Large frames then frames just below the threshold: the walk switches
    /// to windows within the sample ring's length and the miss streak of
    /// the transition.
    #[test]
    fn a_density_change_settles_within_the_sample_ring() {
        let small = usize::try_from(HEADER_WINDOW_DENSE_FRAME_BYTES).expect("threshold") - 1024;
        let mut frames = vec![(2 * 1024 * 1024, 0); 10];
        frames.extend(std::iter::repeat_n((small, 0), 40));
        let (bytes, starts) = raw_file(&frames);
        let reads = walk_logged(bytes, &starts);
        let transition = starts[11];
        assert!(
            reads
                .iter()
                .all(|&(at, len)| at >= transition || len <= HEADER_PROBE_SIZE),
            "a window before the transition: {reads:?}"
        );
        let deadline = starts[11 + HEADER_WINDOW_SAMPLE_FRAMES + FIRST_WINDOW_READ];
        assert!(
            reads
                .iter()
                .any(|&(at, len)| at <= deadline && len > HEADER_PROBE_SIZE),
            "no window within the ring of the transition: {reads:?}"
        );
    }

    /// The frame size the gate sees includes the header: payloads below the
    /// threshold behind headers that carry it above never read windows.
    #[test]
    fn header_bytes_count_toward_frame_density() {
        let (bytes, starts) = raw_file(&[(100 * 1024, 40 * 1024); 30]);
        let reads = walk_logged(bytes, &starts);
        // Probes, plus one exact read of each header's remainder past it.
        assert!(
            reads.iter().all(|&(_, len)| len <= 40 * 1024 + 64),
            "{reads:?}"
        );
    }

    /// A window that ends 1-3 bytes into a length prefix, or inside a
    /// header, is completed by the next read rather than ending the walk.
    /// The cold walk's first window opens at the data frame after the miss
    /// streak, so that frame's size places the window's end inside a uniform
    /// run.
    #[test]
    fn a_window_ending_inside_a_frame_does_not_end_the_walk() {
        let uniform = 10_000;
        let f = frame_len(uniform) as u64;
        for k in [1_u64, 2, 3, 6] {
            // Choose the window frame's size so the window's end lies k
            // bytes into a uniform frame.
            let mut opener = uniform;
            while (HEADER_WINDOW_SIZE as u64 - frame_len(opener) as u64) % f != k {
                opener += 1;
            }
            let mut frames = vec![(uniform, 0); FIRST_WINDOW_READ];
            frames.push((opener, 0));
            frames.extend(std::iter::repeat_n((uniform, 0), 200));
            let (bytes, starts) = raw_file(&frames);
            let reads = walk_logged(bytes, &starts);
            assert!(
                reads
                    .iter()
                    .any(|&(at, _)| starts.iter().any(|&s| at == s + k)),
                "k {k}: no read resumed {k} bytes into a frame: {reads:?}"
            );
        }
    }

    /// A clean tail of 0-3 bytes reads as the end inside a window as well.
    #[test]
    fn a_short_tail_inside_a_window_is_the_end() {
        for tail in 0..4 {
            let (mut bytes, starts) = raw_file(&[(10_000, 0); 20]);
            bytes.extend(std::iter::repeat_n(0xAA, tail));
            walk_logged(bytes, &starts);
        }
    }

    /// A file cut short after a window captured later frames: those frames
    /// are served from the window, and the walk still fails at the next
    /// read inside the captured extent rather than ending cleanly.
    #[test]
    fn a_shrink_after_a_window_still_fails_the_walk() {
        let (bytes, starts) = raw_file(&[(10_000, 0); 300]);
        let host_owner = crate::host::Host::for_tests();
        let host = host_owner.entered();
        let size = bytes.len() as u64;
        let file = Arc::new(LoggedFile::new(bytes));
        let mut walker = super::HeaderWalker::over(
            Arc::clone(&file),
            size,
            std::path::Path::new("cut.pbf"),
            &host,
        )
        .accepting_partial_index();
        // Past the first window's opening frame.
        for _ in 0..=FIRST_WINDOW_READ + 1 {
            walker.next_header().expect("walk").expect("frame");
        }
        assert_eq!(walker.window_stats().windows, 1);
        let cut = 50;
        file.bytes
            .lock()
            .expect("bytes")
            .truncate(usize::try_from(starts[cut]).expect("start"));
        let mut walked = FIRST_WINDOW_READ + 2;
        let err = loop {
            match walker.next_header() {
                Ok(Some(_)) => walked += 1,
                Ok(None) => panic!("a shrunk file ended the walk cleanly"),
                Err(e) => break e,
            }
        };
        assert!(
            walked > cut,
            "the window's frames were not served: {walked}"
        );
        assert!(
            matches!(err.kind(), crate::ErrorKind::Io(io)
                if io.kind() == std::io::ErrorKind::UnexpectedEof),
            "{err:?}"
        );
    }

    /// Open a walk of `path` (`O_DIRECT` when `direct`), then grow the file
    /// by a copy of itself or cut its last ten bytes, then walk it.
    fn walk_after_change(path: &std::path::Path, direct: bool, grow: bool) -> super::Result<usize> {
        let host_owner = crate::host::Host::for_tests();
        let host = host_owner.entered();
        let intact = std::fs::read(path)?;
        let change = || {
            if grow {
                use std::io::Write as _;
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(path)
                    .expect("open to grow");
                file.write_all(&intact).expect("grow");
            } else {
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(path)
                    .expect("open to shrink")
                    .set_len(intact.len() as u64 - 10)
                    .expect("shrink");
            }
        };
        #[cfg(feature = "linux-direct-io")]
        if direct {
            let walker = super::HeaderWalker::open_direct(path, &host)?;
            change();
            return count_frames(walker);
        }
        assert!(!direct, "a direct walk needs linux-direct-io");
        let walker = super::HeaderWalker::open(path, &host)?;
        change();
        count_frames(walker)
    }

    /// The captured extent holds for every transport: bytes appended after
    /// open are not walked, and committed bytes that vanish after open fail
    /// the walk instead of ending it cleanly.
    #[test]
    fn the_walk_keeps_the_extent_captured_at_open() {
        let dir = crate::path_guard::tests::scratch();
        #[cfg_attr(not(feature = "linux-direct-io"), allow(unused_mut))]
        let mut transports = vec![false];
        #[cfg(feature = "linux-direct-io")]
        if crate::test_support::direct_reads_supported(dir.path(), "the direct transport") {
            transports.push(true);
        }
        let path = dir.path().join("extent.pbf");
        for direct in transports {
            write_index_pattern(&path, &[true, true, true]);
            let frames = walk_all(&path, false).expect("intact walk");
            assert_eq!(
                walk_after_change(&path, direct, true).expect("grown walk"),
                frames,
                "direct {direct}: a file grown after open walked further"
            );
            write_index_pattern(&path, &[true, true, true]);
            let err = walk_after_change(&path, direct, false)
                .expect_err("a file shrunk after open must fail the walk");
            assert!(
                matches!(err.kind(), crate::ErrorKind::Io(io)
                    if io.kind() == std::io::ErrorKind::UnexpectedEof),
                "direct {direct}: {err:?}"
            );
        }
    }

    /// A direct walk sees what a buffered one sees: the same metas and the
    /// same payload bytes, frame for frame, over frames at unaligned offsets
    /// and a file whose end is not block-aligned.
    #[cfg(feature = "linux-direct-io")]
    #[test]
    fn a_direct_walk_reads_what_a_buffered_walk_reads() {
        let dir = crate::path_guard::tests::scratch();
        if !crate::test_support::direct_reads_supported(dir.path(), "the whole test") {
            return;
        }
        let path = dir.path().join("walk.pbf");
        write_index_pattern(&path, &[true, false, true, true, false]);
        let host_owner = crate::host::Host::for_tests();
        let host = host_owner.entered();
        // The fixture is partially indexed on purpose (metas with and without
        // an index); the walks compared here are the transports, not the
        // command-input policy.
        let mut buffered = super::HeaderWalker::open(&path, &host)
            .expect("buffered")
            .accepting_partial_index();
        let before = direct_opens();
        let mut direct = super::HeaderWalker::open_direct(&path, &host)
            .expect("direct")
            .accepting_partial_index();
        assert_eq!(direct_opens(), before + 1);
        assert_eq!(direct.file_size(), buffered.file_size());
        let (mut a, mut b) = (
            crate::read::read_buf::ReadBuf::default(),
            crate::read::read_buf::ReadBuf::default(),
        );
        let mut frames = 0;
        while let Some(want) = buffered.next_header().expect("buffered walk") {
            let got = direct.next_header().expect("direct walk").expect("a frame");
            assert_eq!(
                (
                    got.blob_type,
                    got.frame_start,
                    got.blob_index,
                    got.data_offset
                ),
                (
                    want.blob_type,
                    want.frame_start,
                    want.blob_index,
                    want.data_offset
                )
            );
            let index = |i: Option<crate::blob_meta::BlobIndex>| {
                i.map(|i| (i.kind, i.min_id, i.max_id, i.count))
            };
            assert_eq!(
                (
                    got.data_size,
                    got.frame_size,
                    index(got.index),
                    &got.tagdata
                ),
                (
                    want.data_size,
                    want.frame_size,
                    index(want.index),
                    &want.tagdata
                )
            );
            let a = buffered
                .pread_data(want.data_offset, want.data_size, &mut a)
                .expect("buffered payload");
            let b = direct
                .pread_data(got.data_offset, got.data_size, &mut b)
                .expect("direct payload");
            assert_eq!(a, b, "payload of frame {frames}");
            frames += 1;
        }
        assert!(direct.next_header().expect("direct end").is_none());
        assert_eq!(frames, 6);
    }
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
        let host_owner = crate::host::Host {
            diagnostics,
            ..crate::host::Host::for_tests()
        };
        let host = host_owner.entered();
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
        let result = super::HeaderWalker::open(&missing, &crate::host::Host::for_tests().entered());

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
        let mut walker =
            super::HeaderWalker::open(path, &crate::host::Host::for_tests().entered())?;
        if !refusing {
            walker = walker.accepting_partial_index();
        }
        let mut n = 0;
        while walker.next_header()?.is_some() {
            n += 1;
        }
        Ok(n)
    }

    /// A partially indexed file is the typed missing-indexdata error, located
    /// at the first data blob without indexdata, whichever kind comes first.
    /// Uniform files pass, and a walk that opted out never refuses.
    #[test]
    fn a_walk_stops_a_partially_indexed_file_at_its_first_unindexed_blob() {
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
        let mut walker =
            super::HeaderWalker::open(&path, &crate::host::Host::for_tests().entered())
                .expect("walker");
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
            let mut walker =
                super::HeaderWalker::open(path, &crate::host::Host::for_tests().entered())?;
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
            estimate_blob_count(&header_only, &crate::host::Host::for_tests().entered())
                .expect("estimate"),
            BlobCountEstimate {
                osmdata_blobs: 0,
                exact: true,
            }
        );

        let one_block = dir.path().join("one-block.pbf");
        write_fixture(&one_block, true);
        assert_eq!(
            estimate_blob_count(&one_block, &crate::host::Host::for_tests().entered())
                .expect("estimate"),
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
        assert!(estimate_blob_count(&empty, &crate::host::Host::for_tests().entered()).is_err());
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

        let estimate = estimate_blob_count(&mixed, &crate::host::Host::for_tests().entered())
            .expect("estimate");
        assert!(!estimate.exact);
        let error = estimate.osmdata_blobs.abs_diff(actual_blobs);
        assert!(
            error * 100 < actual_blobs * 30,
            "estimated {} of {actual_blobs} actual blobs; relative error over 30 %",
            estimate.osmdata_blobs,
        );
    }
}
