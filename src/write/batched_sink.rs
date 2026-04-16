//! Batched buffered output sink — writes framed chunks via `writev(2)`.
//!
//! Accumulates [`OutputChunk`] items in memory and flushes them in a single
//! `writev` syscall when either the byte threshold or the frame threshold is
//! reached. Owns a raw [`File`] directly — no `BufWriter` in the path — so the
//! kernel sees real scatter-gather I/O instead of coalesced per-slice writes.
//!
//! Forked for the step-2 measurement described in
//! `notes/write-path-optimization-plan.md`. The buffered
//! [`FileOutputSink`](super::writer::FileOutputSink) remains the default for
//! every other command.

use std::fs::File;
use std::io::{self, IoSlice, Write};
use std::sync::atomic::Ordering::Relaxed;

use super::metrics::WRITER_METRICS;
use super::should_sync_all;
use super::writer::{OutputChunk, OutputSink};

/// Byte threshold (~1 MiB). Flushes when the accumulated chunks reach this.
const DEFAULT_FLUSH_BYTES: usize = 1 << 20;

/// Frame threshold. Kept well below `IOV_MAX / 3` so even three-slice
/// `Framed` chunks never exceed the kernel's iovec limit in one batch.
const DEFAULT_FLUSH_FRAMES: usize = 256;

fn elapsed_ns_u64(start: std::time::Instant) -> u64 {
    u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

pub(crate) struct BatchedBufferedSink {
    file: File,
    pending: Vec<OutputChunk>,
    pending_bytes: usize,
    flush_threshold_bytes: usize,
    flush_threshold_frames: usize,
}

impl BatchedBufferedSink {
    pub(crate) fn new(file: File) -> Self {
        Self {
            file,
            pending: Vec::with_capacity(DEFAULT_FLUSH_FRAMES),
            pending_bytes: 0,
            flush_threshold_bytes: DEFAULT_FLUSH_BYTES,
            flush_threshold_frames: DEFAULT_FLUSH_FRAMES,
        }
    }

    fn should_flush(&self) -> bool {
        self.pending.len() >= self.flush_threshold_frames
            || self.pending_bytes >= self.flush_threshold_bytes
    }

    fn chunk_byte_len(chunk: &OutputChunk) -> usize {
        match chunk {
            OutputChunk::Framed(parts) => {
                parts.prefix.len() + parts.header.len() + parts.blob.len()
            }
            OutputChunk::Raw(bytes) => bytes.len(),
            OutputChunk::RawChunks(chunks) => chunks.iter().map(Vec::len).sum(),
            #[cfg(feature = "linux-direct-io")]
            OutputChunk::CopyRange { .. } => 0,
        }
    }

    fn flush_batch(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let frames = self.pending.len() as u64;
        let bytes = self.pending_bytes as u64;

        let mut slices: Vec<IoSlice<'_>> = Vec::new();
        for chunk in &self.pending {
            match chunk {
                OutputChunk::Framed(parts) => {
                    slices.push(IoSlice::new(&parts.prefix));
                    slices.push(IoSlice::new(&parts.header));
                    slices.push(IoSlice::new(&parts.blob));
                }
                OutputChunk::Raw(buf) => {
                    slices.push(IoSlice::new(buf));
                }
                OutputChunk::RawChunks(chunks) => {
                    for c in chunks {
                        slices.push(IoSlice::new(c));
                    }
                }
                #[cfg(feature = "linux-direct-io")]
                OutputChunk::CopyRange { .. } => {
                    // CopyRange forces an immediate flush before it arrives;
                    // it should never sit in `pending`. Debug-assert rather
                    // than silently skip, so a future regression is loud.
                    debug_assert!(false, "CopyRange in BatchedBufferedSink::pending");
                }
            }
        }

        let t_write = std::time::Instant::now();
        write_all_vectored_raw(&mut self.file, &mut slices)?;
        WRITER_METRICS
            .write_ns
            .fetch_add(elapsed_ns_u64(t_write), Relaxed);
        WRITER_METRICS.bytes_written.fetch_add(bytes, Relaxed);
        WRITER_METRICS.batched_writev_calls.fetch_add(1, Relaxed);
        WRITER_METRICS
            .batched_writev_frames
            .fetch_add(frames, Relaxed);

        self.pending.clear();
        self.pending_bytes = 0;
        Ok(())
    }
}

impl OutputSink for BatchedBufferedSink {
    fn write_chunk(&mut self, chunk: OutputChunk) -> io::Result<()> {
        match chunk {
            #[cfg(feature = "linux-direct-io")]
            OutputChunk::CopyRange { in_fd, offset, len } => {
                // CopyRange cannot ride in a writev — copy_file_range(2) is a
                // separate syscall, and the kernel has no notion of
                // "flush pending writev first". Force the batch out at the
                // correct file position before issuing the splice.
                self.flush_batch()?;
                use std::os::unix::io::AsRawFd;
                let out_fd = self.file.as_raw_fd();
                let t_write = std::time::Instant::now();
                super::writer::copy_range(in_fd, out_fd, offset, len)?;
                WRITER_METRICS
                    .write_ns
                    .fetch_add(elapsed_ns_u64(t_write), Relaxed);
                WRITER_METRICS.bytes_written.fetch_add(len, Relaxed);
                Ok(())
            }
            other => {
                self.pending_bytes += Self::chunk_byte_len(&other);
                self.pending.push(other);
                if self.should_flush() {
                    self.flush_batch()?;
                }
                Ok(())
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_batch()?;
        // File has no userspace buffer, so there is nothing else to drain;
        // honor the repo's end-of-command durability policy for parity with
        // `FileOutputSink`.
        if should_sync_all() {
            let t_sync = std::time::Instant::now();
            self.file.sync_all()?;
            WRITER_METRICS
                .sync_all_ns
                .fetch_add(elapsed_ns_u64(t_sync), Relaxed);
        }
        Ok(())
    }
}

/// Drive [`Write::write_vectored`] to completion, handling partial writes.
///
/// `std::io::Write::write_all_vectored` is nightly-only; this is the stable
/// equivalent. Mutates `bufs` via [`IoSlice::advance_slices`] to track the
/// remaining unwritten region without allocating a new slice Vec per round.
fn write_all_vectored_raw<W: Write>(w: &mut W, bufs: &mut [IoSlice<'_>]) -> io::Result<()> {
    let mut bufs: &mut [IoSlice<'_>] = bufs;
    while !bufs.is_empty() {
        let n = w.write_vectored(bufs)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "write_vectored returned 0",
            ));
        }
        IoSlice::advance_slices(&mut bufs, n);
    }
    Ok(())
}
