//! PBF write path: block building, blob framing, and the output backends.
//!
//! # Four backends, deliberately not unified
//!
//! `FileWriter` (buffered), `DirectWriter` (`O_DIRECT`), `UringState`
//! (`io_uring`) and `parallel_writer`'s `pwrite` pool sit behind the
//! `OutputSink` boundary as four separate implementations. That is a
//! decision, not drift - see `decisions/0014-output-backends-stay-separate.md`
//! for why, including the correction that only two of the four ever
//! duplicated the page-alignment logic that unification proposals cite.
//!
//! What the backends must share, they share by name rather than by
//! inheritance: the `SeqTotal` / `check_stream_complete` end-of-stream
//! handshake, the `PermitGuard` write-ahead discipline, the
//! `WriterMetrics::sync_recorded` / `record_recv_wait` funnels, and the
//! `flush`-versus-`sync` split. Each of those is enforced or asserted
//! somewhere concrete; changing one is the trigger to re-read the ADR.
//!
//! The pipelined backends also share one receive loop,
//! `pipeline::deliver_in_order`: receive, reorder, time and count each
//! handoff, prove the stream complete. It touches no bytes, so it is not the
//! unified buffering core the ADR rejects; each backend is an `OutputSink`
//! for its byte path and finalizes on its own after the loop returns.

pub mod block_builder;
pub mod compression;
mod copy_range;
#[cfg(feature = "linux-direct-io")]
pub mod direct_writer;
mod encode;
pub mod file_writer;
mod framing;
pub mod header_builder;
pub(crate) mod parallel_gzip;
pub(crate) mod parallel_writer;
mod pipeline;
mod pool_failure;
mod string_table;

pub(crate) mod metrics;

/// Emit `invocation`'s `writer_*` sidecar counters, at most once per
/// invocation.
///
/// Call this once, on the way out, after every `PbfWriter` of the invocation
/// has been flushed and its outputs published - the CLI does so on its single
/// exit path. Repeat calls are no-ops, so an early-return path may call it
/// too.
///
/// The counters are the invocation's totals rather than per-writer numbers;
/// `write::metrics` says why.
pub fn emit_writer_counters(invocation: &crate::host::Invocation) {
    invocation.metrics().writer.emit_once(invocation);
}

/// Report, once each, the ways the writers' output or speed fell short of
/// the normal path, for users who never read sidecar counters:
///
/// - a warning when output blobs lost their optional tag skip-index
///   (`tagdata`) because their headers exceeded the size limit. Output stays
///   valid; later tag filtering of that file decodes more blobs.
/// - a note when the kernel refused `copy_file_range` for passthrough ranges
///   and they were routed to the userspace copy, naming the first refusal.
///   The note says only that the userspace copy was selected, not that it
///   finished: the count is taken when a range is routed, and this runs on
///   failure too.
///
/// Call after dispatch, beside [`emit_writer_counters`]: the counts are the
/// same invocation totals (of `host.invocation`) behind the
/// `writer_tagdata_dropped_oversize` and `writer_copy_range_fallback_*`
/// counters, reported to `host.diagnostics`.
pub fn report_writer_degradations(host: &crate::host::Host) {
    use std::sync::atomic::Ordering::Relaxed;
    let diagnostics = &host.diagnostics;
    let m = &host.invocation.metrics().writer;
    let dropped = m.tagdata_dropped_oversize.load(Relaxed);
    if dropped != 0 {
        crate::host::diag_once!(
            diagnostics,
            Warning,
            "writer.tagdata_dropped_oversize",
            "{dropped} output blobs lost their tag skip-index because their headers exceeded the size limit; tag filtering may decode more blobs"
        );
    }
    // Only a kernel refusal on Linux moves these counters.
    let ranges = m.copy_range_fallback_ranges.load(Relaxed);
    if ranges != 0 {
        let bytes = m.copy_range_fallback_bytes.load(Relaxed);
        let errno = m.copy_range_fallback_errno.load(Relaxed);
        let name = copy_range::refusal_name(errno).unwrap_or("errno");
        let first = std::io::Error::from_raw_os_error(errno);
        crate::host::diag_once!(
            diagnostics,
            Note,
            "writer.copy_range_fallback",
            "the kernel refused copy_file_range for {ranges} passthrough ranges ({bytes} bytes requested), so they were routed to the userspace copy, which moves every byte through this process; first refusal: {name}, {first}"
        );
    }
}
pub mod options;
#[cfg(feature = "linux-io-uring")]
pub mod uring_writer;
pub mod writer;

pub use options::{OutputBackend, WriteOptions};

/// What a pipelined writer does at end of stream beyond finishing the file
/// (draining, tail padding, truncation to the logical size).
///
/// Only the `fsync` differs. A writer driven by an [`Output`](crate::Output)
/// is `Deferred`: the output's publication syncs the staged file, after every
/// in-process timer has stopped, through the same
/// [`WriterMetrics::sync_recorded`](metrics::WriterMetrics::sync_recorded)
/// funnel. The public path constructors (`PbfWriter::to_path` and friends)
/// stay `Durable`, because a library caller that owns the path has no
/// publication to do it for them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Finish {
    /// `fsync` as the last step of end of stream.
    Durable,
    /// Leave the `fsync` to the owner of the file.
    Deferred,
}

/// Create (or truncate) the file a buffered or `pwrite` backend writes: an
/// [`Output`](crate::Output)'s staging path, or the path a library caller
/// handed a public path constructor. The backends' one plain create; the
/// `O_DIRECT` and `io_uring` backends create through
/// `crate::direct_io::create_for_write`, which also returns the alignment
/// their writes keep. Each caller names the path in its error.
#[allow(clippy::disallowed_methods)]
pub(crate) fn create_backend_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    std::fs::File::create(path)
}

/// Shared buffered and aligned I/O transfer size; owned by `tunables`
/// (which also asserts it is a multiple of the direct I/O alignment floor).
pub(crate) use crate::tunables::IO_BUF_BYTES;

#[cfg(test)]
mod io_buffer_contract;
