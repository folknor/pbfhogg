use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

static ARMED: AtomicBool = AtomicBool::new(false);

/// Enable lazy marker FIFO initialization from the environment.
pub fn enable_marker_fifo_from_env() {
    ARMED.store(true, Relaxed);
}

/// No-op unless `enable_marker_fifo_from_env` has been called.
///
/// Emit a named phase marker to the sidecar profiler (if active).
///
/// The marker is timestamped with monotonic microseconds since process start
/// (on Linux the epoch is recovered from `/proc/self/stat`, so it covers
/// dynamic-loader and pre-`main` time too; elsewhere it is the first FIFO
/// use) and written to the FIFO at `BROKKR_MARKER_FIFO`. If no sidecar is
/// running (env var absent), this is a no-op. If the FIFO buffer is full,
/// the marker is silently dropped (O_NONBLOCK) - never partially written.
pub fn emit_marker(name: &str) {
    write_fifo(|f, us| {
        let mut line = Line::new();
        line.push(format_args!("{us} {name}\n"));
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
        let mut line = Line::new();
        line.push(format_args!("{us} @{name}={value}\n"));
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

/// Read cumulative (minor, major) page faults from `/proc/self/stat`.
/// Returns `(minflt, majflt)`. Returns `(0, 0)` on failure or non-Linux.
#[cfg(target_os = "linux")]
pub fn read_page_faults() -> (u64, u64) {
    if !ARMED.load(Relaxed) {
        return (0, 0);
    }
    let Ok(stat) = std::fs::read_to_string("/proc/self/stat") else {
        return (0, 0);
    };
    parse_page_faults(&stat)
}

#[cfg(target_os = "linux")]
fn parse_page_faults(stat: &str) -> (u64, u64) {
    // `comm` (field 2) can contain spaces; field 10 and field 12 are
    // indexes 7 and 9 after the closing parenthesis.
    let Some((_, after_comm)) = stat.rsplit_once(')') else {
        return (0, 0);
    };
    let mut fields = after_comm.split_whitespace();
    let minflt = fields
        .nth(7)
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    // Skip field 11 (cminflt) to get field 12 (majflt).
    let majflt = fields
        .nth(1)
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    (minflt, majflt)
}

#[cfg(not(target_os = "linux"))]
pub fn read_page_faults() -> (u64, u64) {
    if !ARMED.load(Relaxed) {
        return (0, 0);
    }
    (0, 0)
}

pub(crate) fn page_fault_deltas(before: (u64, u64), after: (u64, u64)) -> (u64, u64) {
    (
        after.0.saturating_sub(before.0),
        after.1.saturating_sub(before.1),
    )
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
const PIPE_BUF: usize = 512; // POSIX minimum; Linux is 4096.

struct Line {
    buf: [u8; PIPE_BUF],
    len: usize,
    overflow: bool,
}

impl Line {
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

/// Microseconds this process has been alive, recovered from
/// `/proc/self/stat` field 22 (`starttime`, in clock ticks since boot) and
/// `/proc/uptime`. Used to anchor the marker clock at real process start
/// rather than at the first marker, which is what the emit docs promise and
/// what brokkr assumes when correlating markers against its `/proc` samples.
#[cfg(target_os = "linux")]
fn micros_since_process_start() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // `comm` (field 2) is parenthesised and may contain spaces; fields after
    // the closing paren are space-separated, so field 22 is index 19 there.
    let after_comm = stat.rsplit_once(')')?.1;
    let starttime_ticks: u64 = after_comm.split_whitespace().nth(19)?.parse().ok()?;
    // SAFETY: sysconf is a libc function safe to call from any thread.
    let ticks_per_sec = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks_per_sec <= 0 {
        return None;
    }
    #[allow(clippy::cast_sign_loss)]
    let ticks_per_sec = ticks_per_sec as u64;
    let uptime = std::fs::read_to_string("/proc/uptime").ok()?;
    let uptime_secs: f64 = uptime.split_whitespace().next()?.parse().ok()?;
    let start_secs = starttime_ticks as f64 / ticks_per_sec as f64;
    let alive = uptime_secs - start_secs;
    if !alive.is_finite() || alive < 0.0 {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some((alive * 1e6) as u64)
}

#[cfg(not(target_os = "linux"))]
fn micros_since_process_start() -> Option<u64> {
    None
}

/// Shared FIFO write logic for markers and counters.
fn write_fifo(f: impl FnOnce(&std::fs::File, u128)) {
    if !ARMED.load(Relaxed) {
        return;
    }
    use std::sync::OnceLock;

    /// The FIFO handle plus the marker-clock epoch: an `Instant` and the
    /// process-age offset already elapsed at that `Instant`.
    static STATE: OnceLock<Option<(std::fs::File, std::time::Instant, u64)>> = OnceLock::new();

    let state = STATE.get_or_init(|| {
        let path = std::env::var("BROKKR_MARKER_FIFO").ok()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let file = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&path)
                .ok()?;
            // Take the age *after* the open so the open's own cost is
            // inside the offset rather than lost between the two reads.
            let now = std::time::Instant::now();
            Some((file, now, micros_since_process_start().unwrap_or(0)))
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            None
        }
    });

    if let Some((file, anchor, age_at_anchor)) = state.as_ref() {
        let us = u128::from(*age_at_anchor) + anchor.elapsed().as_micros();
        f(file, us);
    }
}

#[cfg(test)]
mod tests {
    use super::{Line, PIPE_BUF};

    #[cfg(target_os = "linux")]
    #[test]
    fn page_fault_parser_handles_spaces_in_comm() {
        let stat = "123 (program name with spaces) R 1 2 3 4 5 6 101 102 103 104";
        assert_eq!(super::parse_page_faults(stat), (101, 103));
    }

    #[test]
    fn page_fault_deltas_saturate_when_a_read_fails() {
        assert_eq!(super::page_fault_deltas((8, 20), (15, 3)), (7, 0));
    }

    #[test]
    fn line_formats_marker_exactly() {
        let mut line = Line::new();
        line.push(format_args!("{} {}\n", 42_u128, "PASS1_END"));
        assert_eq!(&line.buf[..line.len], b"42 PASS1_END\n");
        assert!(!line.overflow);
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

        let sink = tempfile::tempfile().expect("scratch file");
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
        let sink = tempfile::tempfile().expect("scratch file");
        line.emit(&sink);
        assert_eq!(
            sink.metadata().expect("stat sink").len(),
            b"42 PASS1_END\n".len() as u64
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn process_start_epoch_is_recoverable() {
        let us = super::micros_since_process_start().expect("readable on Linux");
        assert!(
            us < 86_400 * 1_000_000,
            "process age {us} us is implausible"
        );
    }
}
