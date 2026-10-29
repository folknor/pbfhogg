//! Instrumentation: where a run's phase markers and counter samples go.
//!
//! Every marker and counter the library emits is emitted under the
//! [`Invocation`](crate::host::Invocation) of the run that produced it, to
//! that invocation's [`InstrumentSink`], if it has one. The library holds no
//! instrumentation state of its own (ADR-0016): the sink, and whatever
//! process resource it writes to, belongs to the caller. [`MarkerFifo`] is the
//! sink the sidecar profiler reads, which the CLI hands its invocation.

use crate::tunables::PIPE_BUF;

/// Receives one invocation's phase markers and counter samples.
///
/// Called synchronously on whatever thread emits, from many threads at
/// once, including during unwinding (the end-of-run emitters run from
/// destructors) and while the emitting code holds a lock that orders its
/// samples (so a sink sees one category's totals in increasing order). So a
/// sink must be prompt and non-blocking, and must not run library work under
/// the same invocation (re-entering the library from a callback can
/// deadlock on those locks). A panic in a callback is contained: the
/// invocation stops calling that sink and the run carries on. A sink's own
/// destructor runs when the last clone of its invocation drops, and must not
/// panic.
///
/// A callback can run on a worker of a rayon pool the library built for the
/// call (merge-changes emits from its `Parallelism::Fixed` pool's workers).
/// Those pools are joined before the call ends, unwinding included (see
/// [`Executor`](crate::host::Executor)), so whatever a callback leaves behind
/// on such a worker is finished first: rayon work it submitted to the current
/// pool (`rayon::spawn`, `rayon::spawn_broadcast` and the like) and the
/// destruction of thread-local values it created, with everything they own.
/// None of that may wait for the call to end, or for anything the invoking
/// thread holds until it does, or the call never ends.
///
/// Calls carry no invocation identity: a caller that funnels several
/// invocations into one collector gives each its own sink that tags what it
/// forwards ([`CallSink`] does this for the sidecar FIFO). Markers are point-in-time names; counters are `(name, value)`
/// samples whose meaning is the name's (most are running totals that only
/// grow, some are per-span deltas emitted repeatedly). Within one thread
/// calls arrive in emission order; across threads nothing is ordered.
///
/// Some values are observations of the whole process taken at a phase
/// boundary of this invocation (the `*_mallinfo_*` allocator fields, the
/// page-fault deltas): a concurrent invocation's work shows in them too.
pub trait InstrumentSink: Send + Sync {
    /// A named phase marker.
    fn marker(&self, name: &str);
    /// A counter sample.
    fn counter(&self, name: &str, value: i64);
}

/// The sidecar profiler's FIFO as an [`InstrumentSink`]: one line per event,
/// `<timestamp_us> <name>` for a marker and `<timestamp_us> @<name>=<value>`
/// for a counter. This is the process stream; [`call`](Self::call) hands out
/// a [`CallSink`] per invocation for the tagged call stream on the same FIFO.
///
/// Timestamps are monotonic microseconds since process start (on Linux the
/// epoch is recovered from `/proc/self/stat`, so it covers dynamic-loader
/// and pre-`main` time too; elsewhere it is the moment the FIFO was opened),
/// anchored once at [`open`](Self::open), so every invocation sharing one
/// `MarkerFifo` - and every [`CallSink`] made from it - shares its clock,
/// which [`now_us`](Self::now_us) reads. The FIFO is opened `O_NONBLOCK` and
/// each line is one `write` of at most `PIPE_BUF` (512) bytes including its
/// newline, which a FIFO delivers whole or not at all: a full FIFO drops the
/// line, never a part of it.
///
/// A line the sidecar reader would reject, misread or alter is dropped
/// whole rather than written: a marker name that is empty, contains a
/// newline, ends in whitespace or starts with `@`; on the untagged stream, a
/// marker name starting with `%` (that prefix means a call tag); a counter
/// name that is empty or contains a newline or `=`; a line over 512 bytes.
/// The library's own names never hit these.
///
/// Cloning is cheap and shares the descriptor and the clock.
#[derive(Clone)]
pub struct MarkerFifo {
    shared: std::sync::Arc<FifoShared>,
}

struct FifoShared {
    file: std::fs::File,
    anchor: std::time::Instant,
    age_at_anchor: u64,
}

impl MarkerFifo {
    /// Open the FIFO at `path`, supplied by the caller (the CLI reads it from
    /// its environment). The bare `io::Error` goes back to the caller, which
    /// names it.
    ///
    /// # Errors
    ///
    /// The open failure; off Unix, always `Unsupported`.
    pub fn open(path: &std::path::Path) -> std::io::Result<Self> {
        let file = open_marker_fifo(path)?;
        // Procfs recovers the true process epoch on Linux. If that probe is
        // unavailable, zero age plus `Instant` still supplies a monotonic
        // FIFO timestamp and preserves the wire format. The age is sampled
        // before the anchor is taken: the other order would count any delay
        // between the two (a preemption, a slow procfs read) both in the age
        // and in every later `elapsed()`, so every reading would be late by it.
        let age_at_anchor = crate::process::micros_since_process_start().unwrap_or(0);
        let anchor = std::time::Instant::now();
        Ok(Self {
            shared: std::sync::Arc::new(FifoShared {
                file,
                anchor,
                age_at_anchor,
            }),
        })
    }

    /// The current time on this FIFO's clock, in the microseconds its lines
    /// carry, so a harness can record a call's start and end on the same
    /// timeline as its markers.
    #[must_use]
    pub fn now_us(&self) -> u64 {
        self.shared.now_us()
    }

    /// A sink for one invocation that writes this FIFO's call stream,
    /// `<timestamp_us> %<call_id> <name>` and
    /// `<timestamp_us> %<call_id> @<name>=<value>`, through the same
    /// descriptor and clock. The caller keeps `id` unique among the calls of
    /// one process run; the reader cannot check that.
    #[must_use]
    pub fn call(&self, id: CallId) -> CallSink {
        CallSink {
            shared: std::sync::Arc::clone(&self.shared),
            id,
            drops: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }
}

impl FifoShared {
    fn now_us(&self) -> u64 {
        let elapsed = u64::try_from(self.anchor.elapsed().as_micros()).unwrap_or(u64::MAX);
        self.age_at_anchor.saturating_add(elapsed)
    }
}

impl InstrumentSink for MarkerFifo {
    fn marker(&self, name: &str) {
        let shared = &self.shared;
        // The process stream keeps no account: a dropped line is gone.
        let _delivered = Line::marker(shared.now_us(), None, name).emit(&shared.file);
    }

    fn counter(&self, name: &str, value: i64) {
        let shared = &self.shared;
        let _delivered = Line::counter(shared.now_us(), None, name, value).emit(&shared.file);
    }
}

/// The identity of one call on a [`MarkerFifo`]'s call stream: 1 to 64
/// bytes of `A-Z a-z 0-9 _ . -`, so it is never escaped.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CallId {
    bytes: [u8; CALL_ID_MAX],
    len: u8,
}

/// The sidecar wire format's call-id length limit.
const CALL_ID_MAX: usize = 64;

impl CallId {
    /// Validate `id`.
    ///
    /// # Errors
    ///
    /// `InvalidArgument` when `id` is empty, longer than 64 bytes, or holds a
    /// byte outside `A-Z a-z 0-9 _ . -`.
    pub fn new(id: &str) -> crate::Result<Self> {
        let raw = id.as_bytes();
        let valid = !raw.is_empty()
            && raw.len() <= CALL_ID_MAX
            && raw
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
        if !valid {
            return Err(crate::Error::invalid_argument(
                "a call id is 1 to 64 bytes of A-Z a-z 0-9 _ . -",
            ));
        }
        let mut bytes = [0; CALL_ID_MAX];
        bytes[..raw.len()].copy_from_slice(raw);
        Ok(Self {
            bytes,
            // Bounded by CALL_ID_MAX above.
            len: u8::try_from(raw.len()).unwrap_or(u8::MAX),
        })
    }

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        // Validated ASCII at construction.
        std::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or("")
    }
}

/// One invocation's sink on a [`MarkerFifo`]'s call stream, made by
/// [`MarkerFifo::call`]. Give each invocation its own (one id per call);
/// every `CallSink` of one FIFO shares its descriptor and clock, and its
/// lines follow the same whole-line and name rules, with the tag counted in
/// the 512 bytes.
///
/// The sink counts every line it dropped ([`dropped`](Self::dropped)): a
/// name the reader would refuse, a line over 512 bytes, and a write that did
/// not take the whole line (a full FIFO, a reader gone). The count belongs
/// to this sink and its clones, not to the FIFO: another call's sink keeps
/// its own.
#[derive(Clone)]
pub struct CallSink {
    shared: std::sync::Arc<FifoShared>,
    id: CallId,
    drops: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl CallSink {
    /// This sink's call id.
    #[must_use]
    pub fn id(&self) -> &CallId {
        &self.id
    }

    /// How many lines this sink (any of its clones) has dropped so far,
    /// saturating at `u64::MAX`. Final once nothing emits through it any
    /// more.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.drops.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Count `line` as dropped unless it went out whole. Only a drop touches
    /// the counter.
    fn account(&self, delivered: bool) {
        use std::sync::atomic::Ordering::Relaxed;
        if !delivered {
            // `Err` only when already saturated, which is where it stays.
            let _saturated = self
                .drops
                .try_update(Relaxed, Relaxed, |n| n.checked_add(1));
        }
    }
}

impl InstrumentSink for CallSink {
    fn marker(&self, name: &str) {
        let shared = &self.shared;
        self.account(Line::marker(shared.now_us(), Some(&self.id), name).emit(&shared.file));
    }

    fn counter(&self, name: &str, value: i64) {
        let shared = &self.shared;
        self.account(
            Line::counter(shared.now_us(), Some(&self.id), name, value).emit(&shared.file),
        );
    }
}

/// Whether the reader takes `name` back verbatim as a marker: it trims the
/// line, splits off the timestamp (and call tag) once, reads a body starting
/// `@` as a counter and an untagged payload starting `%` as a tag.
fn marker_name_ok(name: &str, tagged: bool) -> bool {
    !name.is_empty()
        && !name.contains('\n')
        && !name.ends_with(char::is_whitespace)
        && !name.starts_with('@')
        && (tagged || !name.starts_with('%'))
}

/// Whether the reader takes `name` back verbatim as a counter name: it
/// splits the body at the first `=`.
fn counter_name_ok(name: &str) -> bool {
    !name.is_empty() && !name.contains('\n') && !name.contains('=')
}

/// The sidecar FIFO is the caller's instrumentation channel, not an input or
/// an output; the bare `io::Error` goes back to the caller, which names it.
#[cfg(unix)]
#[allow(clippy::disallowed_methods)]
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

mod sealed {
    pub trait Sealed {}
}

/// A value [`Invocation`](crate::host::Invocation)'s counter emission
/// accepts: the signed integers it sends exactly, the unsigned ones it
/// saturates at `i64::MAX`, and `bool` as 0 or 1.
pub trait CounterValue: sealed::Sealed {
    /// The value on the wire.
    fn to_counter(self) -> i64;
}

macro_rules! exact_counter_value {
    ($($t:ty),*) => {$(
        impl sealed::Sealed for $t {}
        impl CounterValue for $t {
            #[inline]
            fn to_counter(self) -> i64 {
                i64::from(self)
            }
        }
    )*};
}
exact_counter_value!(i64, i32, u32, bool);

macro_rules! saturating_counter_value {
    ($($t:ty),*) => {$(
        impl sealed::Sealed for $t {}
        impl CounterValue for $t {
            #[inline]
            fn to_counter(self) -> i64 {
                i64::try_from(self).unwrap_or(i64::MAX)
            }
        }
    )*};
}
saturating_counter_value!(u64, usize);

/// Snapshot glibc allocator state via `mallinfo2()` and emit the key fields
/// as counters of `invocation` with `<prefix>_<field>` names.
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
/// The allocator is the process's: these are process observations taken at
/// one of this invocation's phase boundaries. Brokkr stores application
/// counters by their supplied names and reports added, missing, or changed
/// counters in comparisons. This function does no allocator query unless the
/// invocation is instrumented.
///
/// On non-glibc platforms this is a no-op.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub(crate) fn emit_mallinfo2(invocation: &crate::host::Recorder, prefix: &str) {
    if !invocation.instrumented() {
        return;
    }
    // SAFETY: mallinfo2 is a glibc function safe to call from any thread.
    let info = unsafe { libc::mallinfo2() };
    invocation.counter(&format!("{prefix}_arena"), info.arena);
    invocation.counter(&format!("{prefix}_hblks"), info.hblks);
    invocation.counter(&format!("{prefix}_hblkhd"), info.hblkhd);
    invocation.counter(&format!("{prefix}_uordblks"), info.uordblks);
    invocation.counter(&format!("{prefix}_fordblks"), info.fordblks);
    invocation.counter(&format!("{prefix}_keepcost"), info.keepcost);
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub(crate) fn emit_mallinfo2(_invocation: &crate::host::Recorder, _prefix: &str) {}

/// Read the process's cumulative (minor, major) page faults for an
/// instrumented invocation; `None` when it is not instrumented or the procfs
/// sample is unavailable.
pub(crate) fn read_page_faults(invocation: &crate::host::Recorder) -> Option<(u64, u64)> {
    if !invocation.instrumented() {
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

/// Publish a stage's fault deltas under the two counter names of
/// `invocation`, or nothing when either sample is missing. The faults are the
/// process's, between two of this invocation's phase boundaries.
pub(crate) fn emit_page_fault_deltas(
    invocation: &crate::host::Recorder,
    names: (&str, &str),
    before: Option<(u64, u64)>,
    after: Option<(u64, u64)>,
) {
    if let Some((minor, major)) = page_fault_deltas(before, after) {
        invocation.counter(names.0, minor);
        invocation.counter(names.1, major);
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
    fn marker(us: u64, call: Option<&CallId>, name: &str) -> Self {
        let mut line = Self::new();
        if !marker_name_ok(name, call.is_some()) {
            line.overflow = true;
            return line;
        }
        line.stamp(us, call);
        line.push(format_args!("{name}\n"));
        line
    }

    fn counter(us: u64, call: Option<&CallId>, name: &str, value: i64) -> Self {
        let mut line = Self::new();
        if !counter_name_ok(name) {
            line.overflow = true;
            return line;
        }
        line.stamp(us, call);
        line.push(format_args!("@{name}={value}\n"));
        line
    }

    /// The timestamp and optional call tag, each followed by a space. The
    /// reader parses the timestamp as `i64`, so a larger one drops the line.
    fn stamp(&mut self, us: u64, call: Option<&CallId>) {
        if i64::try_from(us).is_err() {
            self.overflow = true;
            return;
        }
        self.push(format_args!("{us} "));
        if let Some(id) = call {
            self.push(format_args!("%{} ", id.as_str()));
        }
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
    /// Whether the whole line went out.
    #[must_use]
    fn emit(&self, file: &std::fs::File) -> bool {
        use std::io::Write;
        if self.overflow {
            return false;
        }
        // Deliberately `write`, not `write_all`: a short write must drop the
        // marker, not resume it mid-line.
        matches!((&*file).write(&self.buf[..self.len]), Ok(n) if n == self.len)
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
        let line = Line::marker(42, None, "PASS1_END");
        assert_eq!(&line.buf[..line.len], b"42 PASS1_END\n");
        assert!(!line.overflow);
    }

    #[test]
    fn line_formats_counter_exactly() {
        let line = Line::counter(42, None, "nodes", -7);
        assert_eq!(&line.buf[..line.len], b"42 @nodes=-7\n");
        assert!(!line.overflow);
    }

    #[test]
    fn tagged_lines_carry_the_call_id() {
        let id = super::CallId::new("call-1.a_B").expect("valid id");
        let line = Line::marker(42, Some(&id), "PASS1_END");
        assert_eq!(&line.buf[..line.len], b"42 %call-1.a_B PASS1_END\n");
        let line = Line::counter(42, Some(&id), "nodes", i64::MIN);
        assert_eq!(
            &line.buf[..line.len],
            b"42 %call-1.a_B @nodes=-9223372036854775808\n"
        );
        // The reader strips the tag once, so a tagged name may start with %.
        let line = Line::marker(42, Some(&id), "%x");
        assert_eq!(&line.buf[..line.len], b"42 %call-1.a_B %x\n");
    }

    #[test]
    fn call_ids_follow_the_wire_alphabet() {
        assert!(super::CallId::new(&"a".repeat(64)).is_ok());
        for bad in ["", "a b", "a%", "\u{e9}", "a\n", &"a".repeat(65)] {
            assert!(super::CallId::new(bad).is_err(), "{bad:?} must be refused");
        }
    }

    /// Names the reader would reject, misread or alter are dropped whole.
    #[test]
    fn names_the_reader_would_misread_are_dropped() {
        let id = super::CallId::new("c").expect("valid id");
        for name in ["", "a\nb", "END ", "@x", "%x"] {
            assert!(Line::marker(1, None, name).overflow, "untagged {name:?}");
        }
        for name in ["", "a\nb", "END\t", "@x"] {
            assert!(Line::marker(1, Some(&id), name).overflow, "tagged {name:?}");
        }
        for name in ["", "a=b", "a\n"] {
            assert!(Line::counter(1, None, name, 0).overflow, "counter {name:?}");
        }
        // Leading whitespace survives the reader's parse, so it is kept.
        assert!(!Line::marker(1, None, " x").overflow);
        assert!(!Line::counter(1, None, " x ", 0).overflow);
        assert!(Line::marker(u64::MAX, None, "x").overflow, "ts beyond i64");
    }

    /// Exactly 512 bytes including the newline and the tag goes out; 513
    /// drops the line.
    #[test]
    fn tagged_line_limit_includes_tag_and_newline() {
        let id = super::CallId::new(&"i".repeat(64)).expect("valid id");
        let fixed = "42 ".len() + 1 + 64 + 1 + 1;
        let fits = "n".repeat(PIPE_BUF - fixed);
        let line = Line::marker(42, Some(&id), &fits);
        assert!(!line.overflow);
        assert_eq!(line.len, PIPE_BUF);
        let over = "n".repeat(PIPE_BUF - fixed + 1);
        assert!(Line::marker(42, Some(&id), &over).overflow);
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
        line.push(format_args!("{} {name}\n", 42_u64));

        let scratch = crate::path_guard::tests::scratch();
        let sink = tempfile::tempfile_in(scratch.path()).expect("scratch file");
        assert!(!line.emit(&sink), "an overlong line reports a drop");
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
        assert!(line.emit(&sink), "a whole line reports delivery");
        assert_eq!(
            sink.metadata().expect("stat sink").len(),
            b"42 PASS1_END\n".len() as u64
        );
    }

    /// A call sink counts every line it drops, refused names, overlong lines
    /// and writes a full FIFO turned away alike; its clones share the count,
    /// another call's sink keeps its own.
    #[cfg(target_os = "linux")]
    #[test]
    fn call_sinks_count_their_own_drops() {
        use super::InstrumentSink as _;
        use std::io::Write as _;
        use std::os::unix::ffi::OsStrExt as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let scratch = crate::path_guard::tests::scratch();
        let path = scratch.path().join("fifo");
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("no NUL");
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        // A reader that never reads keeps the FIFO open for writers.
        #[allow(clippy::disallowed_methods)]
        let _reader = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .expect("open the read end");
        let fifo = super::MarkerFifo::open(&path).expect("open the FIFO");
        let a = fifo.call(super::CallId::new("a").expect("id"));
        let a2 = a.clone();
        let b = fifo.call(super::CallId::new("b").expect("id"));

        a.marker("@refused");
        a2.counter("x=y", 1);
        a.marker(&"n".repeat(PIPE_BUF));
        b.marker("DELIVERED");
        assert_eq!((a.dropped(), a2.dropped(), b.dropped()), (3, 3, 0));

        // Fill the FIFO until the kernel refuses a write, then every line
        // is turned away.
        #[allow(clippy::disallowed_methods)]
        let mut filler = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .expect("open a filler");
        let chunk = [b'x'; 4096];
        loop {
            match filler.write(&chunk) {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("filling the FIFO: {e}"),
            }
        }
        // The last bytes may still fit a short line; drain them with
        // single bytes until even one is refused.
        while filler.write(b"x").is_ok() {}
        b.marker("FULL");
        a2.marker("FULL");
        assert_eq!((a.dropped(), b.dropped()), (4, 1));
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
