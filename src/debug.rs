use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

use crate::tunables::PIPE_BUF;

static ARMED: AtomicBool = AtomicBool::new(false);

type MarkerState = (std::fs::File, std::time::Instant, u64);
static STATE: std::sync::OnceLock<MarkerState> = std::sync::OnceLock::new();

/// Initialize instrumentation from a path supplied by the caller.
///
/// The caller reads its environment and reports an error through its own
/// diagnostics. A failed open leaves initialization retryable.
pub fn initialize_marker_fifo(path: &std::path::Path) -> std::io::Result<()> {
    let file = open_marker_fifo(path)?;
    let state = (
        file,
        std::time::Instant::now(),
        // Procfs recovers the true process epoch on Linux. If that probe is
        // unavailable, zero age plus `Instant` still supplies a monotonic FIFO
        // timestamp and preserves the marker wire format.
        crate::process::micros_since_process_start().unwrap_or(0),
    );
    STATE.set(state).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "marker FIFO already initialized",
        )
    })?;
    ARMED.store(true, Relaxed);
    Ok(())
}

#[cfg(unix)]
fn open_marker_fifo(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
}

#[cfg(not(unix))]
fn open_marker_fifo(_path: &std::path::Path) -> std::io::Result<std::fs::File> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "marker FIFO requires Unix",
    ))
}

/// No-op unless `initialize_marker_fifo` has succeeded.
///
/// Emit a named phase marker to the sidecar profiler (if active).
///
/// The marker is timestamped with monotonic microseconds since process start
/// (on Linux the epoch is recovered from `/proc/self/stat`, so it covers
/// dynamic-loader and pre-`main` time too; elsewhere it is the first FIFO
/// use) and written to the FIFO at the caller-supplied path. Before
/// initialization this is a no-op. If the FIFO buffer is full,
/// the marker is silently dropped (O_NONBLOCK) - never partially written.
pub fn emit_marker(name: &str) {
    write_fifo(|f, us| {
        let line = Line::marker(us, name);
        line.emit(f);
    });
}

/// Emit a named counter value to the sidecar profiler (if active).
///
/// Counters carry application-level data (element counts, resolved counts,
/// skipped blobs) through the same FIFO as phase markers. The `@` prefix
/// distinguishes counters from markers in the protocol.
///
/// Format: `<timestamp_us> @<name>=<value>\n`
pub fn emit_counter(name: &str, value: i64) {
    write_fifo(|f, us| {
        let line = Line::counter(us, name, value);
        line.emit(f);
    });
}

/// Snapshot glibc allocator state via `mallinfo2()` and emit the key fields
/// as counters with `<prefix>_<field>` names.
///
/// DIAGNOSTIC (2026-04-11 round 3): used to distinguish brk arena growth
/// (`arena`) from mmap-backed heap chunks (`hblkhd`) at marker boundaries
/// during the post-PASS1 header scan burst investigation.
///
/// Fields emitted:
/// - `<prefix>_arena`     - total brk-managed heap size in bytes
/// - `<prefix>_hblks`     - count of mmap-managed chunks
/// - `<prefix>_hblkhd`    - total bytes in mmap-managed chunks
/// - `<prefix>_uordblks`  - bytes allocated in normal blocks (live)
/// - `<prefix>_fordblks`  - bytes free in normal blocks (free-list)
/// - `<prefix>_keepcost`  - top-most releasable block in arena
///
/// Brokkr stores application counters by their supplied names and reports
/// added, missing, or changed counters in comparisons. This function does no
/// allocator query unless the caller initialized the marker FIFO.
///
/// On non-glibc platforms this is a no-op.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub fn emit_mallinfo2(prefix: &str) {
    if !ARMED.load(Relaxed) {
        return;
    }
    // SAFETY: mallinfo2 is a glibc function safe to call from any thread.
    let info = unsafe { libc::mallinfo2() };
    #[allow(clippy::cast_possible_wrap)]
    {
        emit_counter(&format!("{prefix}_arena"), info.arena as i64);
        emit_counter(&format!("{prefix}_hblks"), info.hblks as i64);
        emit_counter(&format!("{prefix}_hblkhd"), info.hblkhd as i64);
        emit_counter(&format!("{prefix}_uordblks"), info.uordblks as i64);
        emit_counter(&format!("{prefix}_fordblks"), info.fordblks as i64);
        emit_counter(&format!("{prefix}_keepcost"), info.keepcost as i64);
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub fn emit_mallinfo2(_prefix: &str) {}

/// Read cumulative (minor, major) page faults for the active sidecar run;
/// `None` when no sidecar is armed or the procfs sample is unavailable.
pub fn read_page_faults() -> Option<(u64, u64)> {
    if !ARMED.load(Relaxed) {
        return None;
    }
    crate::process::read_page_faults()
}

/// The (minor, major) fault deltas between two samples, or `None` when
/// either is missing: a failed sample omits the stage's counters rather
/// than publishing a delta against zero.
pub(crate) fn page_fault_deltas(
    before: Option<(u64, u64)>,
    after: Option<(u64, u64)>,
) -> Option<(u64, u64)> {
    let (before, after) = (before?, after?);
    Some((
        after.0.saturating_sub(before.0),
        after.1.saturating_sub(before.1),
    ))
}

/// Publish a stage's fault deltas under the two counter names, or nothing
/// when either sample is missing.
pub(crate) fn emit_page_fault_deltas(
    names: (&str, &str),
    before: Option<(u64, u64)>,
    after: Option<(u64, u64)>,
) {
    if let Some((minor, major)) = page_fault_deltas(before, after) {
        emit_counter(names.0, i64::try_from(minor).unwrap_or(i64::MAX));
        emit_counter(names.1, i64::try_from(major).unwrap_or(i64::MAX));
    }
}

/// A protocol line assembled on the stack.
///
/// The FIFO protocol is line-oriented, so a partially written line corrupts
/// brokkr's stream for every marker after it. POSIX guarantees a write of at
/// most `PIPE_BUF` bytes to a pipe/FIFO is atomic - on an `O_NONBLOCK` fd it
/// either transfers every byte or transfers nothing and fails with `EAGAIN`.
/// This buffer therefore refuses to emit anything longer than `PIPE_BUF`, and
/// emits with a single `write` whose short/failed result drops the marker
/// rather than resuming it. Assembling into a fixed array also keeps the hot
/// path allocation-free (the previous code did one `format!` per marker).
struct Line {
    buf: [u8; PIPE_BUF],
    len: usize,
    overflow: bool,
}

impl Line {
    fn marker(us: u128, name: &str) -> Self {
        let mut line = Self::new();
        line.push(format_args!("{us} {name}\n"));
        line
    }

    fn counter(us: u128, name: &str, value: i64) -> Self {
        let mut line = Self::new();
        line.push(format_args!("{us} @{name}={value}\n"));
        line
    }

    fn new() -> Self {
        Self {
            buf: [0; PIPE_BUF],
            len: 0,
            overflow: false,
        }
    }

    /// Append formatted text. An overlong line marks itself undeliverable
    /// rather than truncating.
    fn push(&mut self, args: std::fmt::Arguments) {
        if std::fmt::Write::write_fmt(self, args).is_err() {
            self.overflow = true;
        }
    }

    /// Write the assembled line, or drop it whole. Never writes a prefix.
    fn emit(&self, file: &std::fs::File) {
        use std::io::Write;
        if self.overflow {
            return;
        }
        // Deliberately `write`, not `write_all`: a short write must drop the
        // marker, not resume it mid-line.
        drop((&*file).write(&self.buf[..self.len]));
    }
}

impl std::fmt::Write for Line {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let bytes = s.as_bytes();
        if self.overflow || self.len + bytes.len() > self.buf.len() {
            self.overflow = true;
            return Err(std::fmt::Error);
        }
        self.buf[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
        Ok(())
    }
}

/// Shared FIFO write logic for markers and counters.
fn write_fifo(f: impl FnOnce(&std::fs::File, u128)) {
    if !ARMED.load(Relaxed) {
        return;
    }
    if let Some((file, anchor, age_at_anchor)) = STATE.get() {
        let us = u128::from(*age_at_anchor) + anchor.elapsed().as_micros();
        f(file, us);
    }
}

#[cfg(test)]
mod tests {
    use super::{Line, PIPE_BUF};

    #[test]
    fn page_fault_deltas_are_omitted_when_a_sample_is_missing() {
        assert_eq!(
            super::page_fault_deltas(Some((8, 20)), Some((15, 23))),
            Some((7, 3))
        );
        assert_eq!(super::page_fault_deltas(None, Some((15, 23))), None);
        assert_eq!(super::page_fault_deltas(Some((8, 20)), None), None);
        assert_eq!(
            super::page_fault_deltas(Some((8, 20)), Some((15, 3))),
            Some((7, 0)),
            "a counter that moves backwards saturates rather than underflowing"
        );
    }

    #[test]
    fn line_formats_marker_exactly() {
        let line = Line::marker(42, "PASS1_END");
        assert_eq!(&line.buf[..line.len], b"42 PASS1_END\n");
        assert!(!line.overflow);
    }

    #[test]
    fn line_formats_counter_exactly() {
        let line = Line::counter(42, "nodes", -7);
        assert_eq!(&line.buf[..line.len], b"42 @nodes=-7\n");
        assert!(!line.overflow);
    }

    #[test]
    fn marker_open_failure_is_returned_to_caller() {
        let dir = crate::path_guard::tests::scratch();
        let error = super::open_marker_fifo(&dir.path().join("missing"))
            .expect_err("missing FIFO must be diagnosed by caller");
        #[cfg(unix)]
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        #[cfg(not(unix))]
        assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    }

    /// The failure that matters is not the flag, it is bytes reaching the
    /// wire: a truncated marker desynchronises brokkr's line-oriented
    /// parser for the rest of the run. So assert on what `emit` actually
    /// wrote, and assert it before anything cheaper.
    #[test]
    fn line_over_pipe_buf_emits_nothing_at_all() {
        let name = "X".repeat(PIPE_BUF);
        let mut line = Line::new();
        line.push(format_args!("{} {name}\n", 42_u128));

        let scratch = crate::path_guard::tests::scratch();
        let sink = tempfile::tempfile_in(scratch.path()).expect("scratch file");
        line.emit(&sink);
        assert_eq!(
            sink.metadata().expect("stat sink").len(),
            0,
            "an overlong marker must not put a prefix on the wire"
        );
        assert!(line.overflow, "oversized line must be marked undeliverable");
    }

    /// A line that fits is written in full by the same `emit`, so the
    /// no-op above is a property of overflow and not of `emit` being inert.
    #[test]
    fn line_within_pipe_buf_emits_in_full() {
        let mut line = Line::new();
        line.push(format_args!("{} PASS1_END\n", 42_u128));
        let scratch = crate::path_guard::tests::scratch();
        let sink = tempfile::tempfile_in(scratch.path()).expect("scratch file");
        line.emit(&sink);
        assert_eq!(
            sink.metadata().expect("stat sink").len(),
            b"42 PASS1_END\n".len() as u64
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn process_start_epoch_is_recoverable() {
        let us = crate::process::micros_since_process_start().expect("readable on Linux");
        assert!(
            us < 86_400 * 1_000_000,
            "process age {us} us is implausible"
        );
    }
}
