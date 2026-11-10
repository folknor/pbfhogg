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
/// deadlock on those locks). The one exception is
/// [`Invocation::cancel`](crate::host::Invocation::cancel), which a
/// callback may call on its own invocation: it takes only the publication
/// log's lock, under which nothing runs but the log's own bookkeeping, and
/// never waits for a publication in progress. A panic in a callback is
/// contained: the invocation stops calling that sink and the run carries
/// on. A sink's own
/// destructor runs when the last clone of its invocation drops, and must not
/// panic. Every callback runs inside a call or under a permit of its
/// invocation, so a seal of that invocation cannot happen while one is
/// running; a seal attempted from inside a callback is refused as a call in
/// progress.
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
/// marker name starting with `%` (that prefix means a call tag); on the call
/// stream, the forms reserved for ending it, a marker name starting with `!`
/// and a counter name starting with `@`; a counter name that is empty or
/// contains a newline or `=`; a line over 512 bytes. The library's own names
/// never hit these.
///
/// A write to a FIFO whose reader has gone raises `SIGPIPE`. Rust programs
/// ignore it by default, and the write then fails with `EPIPE`, which drops
/// the line (on a call stream, ending it `failed`). A host that restores the
/// default disposition is killed by its first write after the reader
/// closes, so a program using this sink must keep `SIGPIPE` ignored. The
/// library does not change signal state (ADR-0016).
///
/// Cloning is cheap and shares the descriptor, the clock and the record of
/// the call ids issued.
#[derive(Clone)]
pub struct MarkerFifo {
    shared: std::sync::Arc<FifoShared>,
}

struct FifoShared {
    file: std::fs::File,
    anchor: std::time::Instant,
    age_at_anchor: u64,
    /// Every call id [`MarkerFifo::call`] has issued on this FIFO, kept for
    /// its life so no id is issued twice.
    issued: std::sync::Mutex<std::collections::HashSet<CallId>>,
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
                issued: std::sync::Mutex::default(),
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
    /// descriptor and clock. Hand it to
    /// [`Invocation::with_call_sink`](crate::host::Invocation::with_call_sink),
    /// whose seal ends the stream.
    ///
    /// # Errors
    ///
    /// `InvalidArgument` when `id` was already issued on this FIFO (any clone
    /// of it), even to a sink since dropped: one id names one stream. Ids
    /// issued through a separately opened `MarkerFifo` of the same path are
    /// not seen here; keeping them apart is the caller's.
    pub fn call(&self, id: CallId) -> crate::Result<CallSink> {
        let fresh = self
            .shared
            .issued
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.clone());
        if !fresh {
            return Err(crate::Error::invalid_argument(format!(
                "call id {} was already issued on this marker FIFO",
                id.as_str()
            )));
        }
        Ok(CallSink {
            shared: std::sync::Arc::clone(&self.shared),
            id,
            cell: StreamCell::default(),
        })
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
        let _sent = Line::marker(shared.now_us(), None, name).emit(&shared.file);
    }

    fn counter(&self, name: &str, value: i64) {
        let shared = &self.shared;
        let _sent = Line::counter(shared.now_us(), None, name, value).emit(&shared.file);
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
/// [`MarkerFifo::call`] and owned by the invocation it is handed to
/// ([`Invocation::with_call_sink`](crate::host::Invocation::with_call_sink)).
/// Not `Clone`: its stream has one owner, which ends it. Every `CallSink` of
/// one FIFO shares its descriptor and clock, and its lines follow the same
/// whole-line and name rules, with the tag counted in the 512 bytes.
///
/// The sink keeps its own account ([`account`](Self::account)): every line
/// it dropped (a name the reader would refuse, a line over 512 bytes, a write
/// that did not take the whole line: a full FIFO, a reader gone) and how its
/// stream ended. A successful seal of its invocation ends it: the sink stops
/// admitting ordinary records, writes `<ts> %<call> @@<name>=<value>` for
/// every counter of the seal's snapshot (zeros included, values saturated at
/// `i64::MAX`), then the terminal record `<ts> %<call> !sealed drops=N`, or
/// `!failed drops=N` once any write of the sink failed outright (not a full
/// FIFO), `N` the lines dropped before it. A terminal record that does not go
/// out whole is counted in the account too, so the account is then `N + 1`.
/// A sink that is never sealed stays open.
pub struct CallSink {
    shared: std::sync::Arc<FifoShared>,
    id: CallId,
    cell: StreamCell,
}

/// How a call sink's stream ended, in its [`StreamAccount`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum StreamState {
    /// Not ended: its invocation has not sealed.
    Open,
    /// Ended by a seal, every write of the sink having succeeded or merely
    /// been turned away by a full FIFO.
    Sealed,
    /// Ended by a seal after a write failed outright (the reader gone, say),
    /// or the ending itself failed.
    Failed,
}

/// A call sink's own account: how its stream ended, and every line it
/// dropped, its terminal record included. Final once the state is not
/// [`Open`](StreamState::Open).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct StreamAccount {
    /// How the stream ended.
    pub state: StreamState,
    /// Lines dropped, saturating at `u64::MAX`.
    pub drops: u64,
}

/// The sink's account as it moves. `state` is published with `Release`
/// after `drops` reaches its final value, so a reader that sees an ended
/// state reads the final count.
#[derive(Default)]
struct StreamCell {
    state: std::sync::atomic::AtomicU8,
    drops: std::sync::atomic::AtomicU64,
    /// A write failed outright at some point.
    hard_error: std::sync::atomic::AtomicBool,
}

const CELL_OPEN: u8 = 0;
const CELL_CLOSING: u8 = 1;
const CELL_SEALED: u8 = 2;
const CELL_FAILED: u8 = 3;

impl CallSink {
    /// This sink's call id.
    #[must_use]
    pub fn id(&self) -> &CallId {
        &self.id
    }

    /// The account so far: [`Open`](StreamState::Open) with the lines
    /// dropped up to now until a seal has ended the stream, then final.
    #[must_use]
    pub fn account(&self) -> StreamAccount {
        use std::sync::atomic::Ordering::{Acquire, Relaxed};
        let state = match self.cell.state.load(Acquire) {
            CELL_SEALED => StreamState::Sealed,
            CELL_FAILED => StreamState::Failed,
            _ => StreamState::Open,
        };
        StreamAccount {
            state,
            drops: self.cell.drops.load(Relaxed),
        }
    }

    /// Count one line by how its write went. Only a drop touches the count.
    fn account_line(&self, sent: Sent) {
        use std::sync::atomic::Ordering::Relaxed;
        match sent {
            Sent::Whole => return,
            Sent::Dropped => {}
            Sent::Failed => self.cell.hard_error.store(true, Relaxed),
        }
        // `Err` only when already saturated, which is where it stays.
        let _saturated = self
            .cell
            .drops
            .try_update(Relaxed, Relaxed, |n| n.checked_add(1));
    }

    /// Whether the stream still admits ordinary records. Once a seal has
    /// begun ending it, an ordinary record is ignored and counted nowhere.
    /// The invocation's capabilities already keep any ordinary record from
    /// overlapping a seal; this is a guard, not the barrier.
    fn open(&self) -> bool {
        self.cell.state.load(std::sync::atomic::Ordering::Relaxed) == CELL_OPEN
    }

    /// End the stream with `snapshot`: the sealing emissions, then the
    /// terminal record, on the sealing thread. `hook` runs after the last
    /// sealing emission (a test parks or panics there). Never unwinds: an
    /// unwind before the terminal record is caught, its payload forgotten,
    /// and the stream ends `!failed`; after the terminal write only atomic
    /// stores run, so the account always agrees with a terminal record that
    /// reached the FIFO.
    pub(crate) fn close(&self, snapshot: &crate::host::Snapshot, hook: impl FnOnce()) {
        use std::sync::atomic::Ordering::{Relaxed, Release};
        self.cell.state.store(CELL_CLOSING, Release);
        let shared = &self.shared;
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            for (name, value) in snapshot.iter() {
                let line = Line::sealing(shared.now_us(), &self.id, name, value);
                self.account_line(line.emit(&shared.file));
            }
            hook();
        }));
        let unwound = match unwound {
            Ok(()) => false,
            Err(payload) => {
                std::mem::forget(payload);
                true
            }
        };
        let before = self.cell.drops.load(Relaxed);
        let failed = unwound || self.cell.hard_error.load(Relaxed);
        let terminal = Line::terminal(shared.now_us(), &self.id, failed, before);
        let (failed, drops) = match terminal.emit(&shared.file) {
            Sent::Whole => (failed, before),
            Sent::Dropped => (failed, before.saturating_add(1)),
            Sent::Failed => (true, before.saturating_add(1)),
        };
        self.cell.drops.store(drops, Relaxed);
        self.cell
            .state
            .store(if failed { CELL_FAILED } else { CELL_SEALED }, Release);
    }
}

impl InstrumentSink for CallSink {
    fn marker(&self, name: &str) {
        if !self.open() {
            return;
        }
        let shared = &self.shared;
        self.account_line(Line::marker(shared.now_us(), Some(&self.id), name).emit(&shared.file));
    }

    fn counter(&self, name: &str, value: i64) {
        if !self.open() {
            return;
        }
        let shared = &self.shared;
        self.account_line(
            Line::counter(shared.now_us(), Some(&self.id), name, value).emit(&shared.file),
        );
    }
}

/// Whether the reader takes `name` back verbatim as a marker: it trims the
/// line, splits off the timestamp (and call tag) once, reads a body starting
/// `@` as a counter and an untagged payload starting `%` as a tag. On the
/// call stream a body starting `!` is a terminal record.
fn marker_name_ok(name: &str, tagged: bool) -> bool {
    !name.is_empty()
        && !name.contains('\n')
        && !name.ends_with(char::is_whitespace)
        && !name.starts_with('@')
        && (tagged || !name.starts_with('%'))
        && !(tagged && name.starts_with('!'))
}

/// Whether the reader takes `name` back verbatim as a counter name: it
/// splits the body at the first `=`. On the call stream a body starting
/// `@@` is a sealing emission.
fn counter_name_ok(name: &str, tagged: bool) -> bool {
    !name.is_empty()
        && !name.contains('\n')
        && !name.contains('=')
        && !(tagged && name.starts_with('@'))
}

/// The protocol's bound on a snapshot counter name, so its sealing emission
/// fits a FIFO line.
const SEALING_NAME_MAX: usize = 400;

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
        if !counter_name_ok(name, call.is_some()) {
            line.overflow = true;
            return line;
        }
        line.stamp(us, call);
        line.push(format_args!("@{name}={value}\n"));
        line
    }

    /// A sealing emission: `<ts> %<call> @@<name>=<value>`, the value
    /// saturated at `i64::MAX` as the wire carries it. A name the protocol
    /// refuses (empty, over 400 bytes, holding `=` or a newline) drops the
    /// line.
    fn sealing(us: u64, call: &CallId, name: &str, value: u64) -> Self {
        let mut line = Self::new();
        if name.is_empty()
            || name.len() > SEALING_NAME_MAX
            || name.contains('\n')
            || name.contains('=')
        {
            line.overflow = true;
            return line;
        }
        let value = i64::try_from(value).unwrap_or(i64::MAX);
        line.stamp(us, Some(call));
        line.push(format_args!("@@{name}={value}\n"));
        line
    }

    /// The terminal record: `<ts> %<call> !sealed drops=N` or
    /// `!failed drops=N`.
    fn terminal(us: u64, call: &CallId, failed: bool, drops: u64) -> Self {
        let mut line = Self::new();
        let state = if failed { "failed" } else { "sealed" };
        line.stamp(us, Some(call));
        line.push(format_args!("!{state} drops={drops}\n"));
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
    /// How the write went.
    #[must_use]
    fn emit(&self, file: &std::fs::File) -> Sent {
        use std::io::Write;
        if self.overflow {
            return Sent::Dropped;
        }
        // Deliberately `write`, not `write_all`: a short write must drop the
        // marker, not resume it mid-line.
        match (&*file).write(&self.buf[..self.len]) {
            Ok(n) if n == self.len => Sent::Whole,
            Ok(_) => Sent::Dropped,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                Sent::Dropped
            }
            Err(_) => Sent::Failed,
        }
    }
}

/// How one line's write went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sent {
    /// The whole line went out.
    Whole,
    /// Nothing went out, and the descriptor is fine: a refused or overlong
    /// line, or a full FIFO.
    Dropped,
    /// The write failed outright (the reader gone, a bad descriptor).
    Failed,
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
        // On the call stream `!` opens a terminal record and `@@` a sealing
        // emission, so neither is an ordinary name there.
        for name in ["", "a\nb", "END\t", "@x", "!sealed drops=0", "!x"] {
            assert!(Line::marker(1, Some(&id), name).overflow, "tagged {name:?}");
        }
        for name in ["", "a=b", "a\n"] {
            assert!(Line::counter(1, None, name, 0).overflow, "counter {name:?}");
        }
        assert!(Line::counter(1, Some(&id), "@x", 0).overflow, "tagged @x");
        // Untagged, `@@x=1` is a counter named `@x` and `!x` a marker.
        assert!(!Line::counter(1, None, "@x", 0).overflow);
        assert!(!Line::marker(1, None, "!x").overflow);
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
        assert_eq!(
            line.emit(&sink),
            super::Sent::Dropped,
            "an overlong line reports a drop"
        );
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
        assert_eq!(
            line.emit(&sink),
            super::Sent::Whole,
            "a whole line reports delivery"
        );
        assert_eq!(
            sink.metadata().expect("stat sink").len(),
            b"42 PASS1_END\n".len() as u64
        );
    }

    #[test]
    fn sealing_and_terminal_lines_follow_the_wire() {
        let id = super::CallId::new("c").expect("valid id");
        let line = Line::sealing(7, &id, "writer_bytes", u64::MAX);
        assert_eq!(
            &line.buf[..line.len],
            b"7 %c @@writer_bytes=9223372036854775807\n"
        );
        let line = Line::terminal(8, &id, false, 0);
        assert_eq!(&line.buf[..line.len], b"8 %c !sealed drops=0\n");
        let line = Line::terminal(8, &id, true, 12);
        assert_eq!(&line.buf[..line.len], b"8 %c !failed drops=12\n");
        for name in ["", "a=b", "a\nb", &"n".repeat(super::SEALING_NAME_MAX + 1)] {
            assert!(Line::sealing(1, &id, name, 0).overflow, "sealing {name:?}");
        }
        // The longest name with the longest id and value still fits.
        let id = super::CallId::new(&"i".repeat(64)).expect("valid id");
        let name = "n".repeat(super::SEALING_NAME_MAX);
        assert!(!Line::sealing((1 << 63) - 1, &id, &name, u64::MAX).overflow);
    }

    /// One id names one stream: a FIFO issues it once, also after the sink
    /// it went to was dropped.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_call_id_is_issued_once() {
        let (_scratch, _reader, fifo) = fifo_with_idle_reader();
        let id = || super::CallId::new("a").expect("id");
        drop(fifo.call(id()).expect("first"));
        assert!(fifo.call(id()).is_err());
        assert!(fifo.clone().call(id()).is_err(), "clones share the record");
        assert!(fifo.call(super::CallId::new("b").expect("id")).is_ok());
    }

    /// A FIFO with a reader that never reads, which keeps it open for
    /// writers.
    #[cfg(target_os = "linux")]
    fn fifo_with_idle_reader() -> (tempfile::TempDir, std::fs::File, super::MarkerFifo) {
        use std::os::unix::ffi::OsStrExt as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let scratch = crate::path_guard::tests::scratch();
        let path = scratch.path().join("fifo");
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("no NUL");
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        #[allow(clippy::disallowed_methods)]
        let reader = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .expect("open the read end");
        let fifo = super::MarkerFifo::open(&path).expect("open the FIFO");
        (scratch, reader, fifo)
    }

    /// Fill `fifo`'s pipe until even one byte is refused.
    #[cfg(target_os = "linux")]
    fn fill(fifo: &super::MarkerFifo) {
        use std::io::Write as _;
        let mut file = &fifo.shared.file;
        let chunk = [b'x'; 4096];
        loop {
            match file.write(&chunk) {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("filling the FIFO: {e}"),
            }
        }
        // The last bytes may still fit a short line; take them with single
        // bytes until even one is refused.
        while file.write(b"x").is_ok() {}
    }

    /// A call sink counts every line it drops, refused names, overlong lines
    /// and writes a full FIFO turned away alike; another call's sink keeps
    /// its own count.
    #[cfg(target_os = "linux")]
    #[test]
    fn call_sinks_count_their_own_drops() {
        use super::InstrumentSink as _;
        let (_scratch, _reader, fifo) = fifo_with_idle_reader();
        let a = fifo.call(super::CallId::new("a").expect("id")).expect("a");
        let b = fifo.call(super::CallId::new("b").expect("id")).expect("b");

        a.marker("@refused");
        a.counter("x=y", 1);
        a.marker(&"n".repeat(PIPE_BUF));
        b.marker("DELIVERED");
        let drops = |s: &super::CallSink| s.account().drops;
        assert_eq!((drops(&a), drops(&b)), (3, 0));

        fill(&fifo);
        b.marker("FULL");
        a.marker("FULL");
        assert_eq!((drops(&a), drops(&b)), (4, 1));
        assert_eq!(a.account().state, super::StreamState::Open);
    }

    fn snapshot(counters: &[(&'static str, u64)]) -> crate::host::Snapshot {
        crate::host::Snapshot::for_tests(counters.to_vec())
    }

    /// A terminal record a full FIFO turned away is counted in the account
    /// (`N + 1` against the `N` the record would have carried), and the
    /// stream still ends sealed: nothing failed outright.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_dropped_terminal_record_is_counted() {
        let (_scratch, _reader, fifo) = fifo_with_idle_reader();
        let sink = fifo.call(super::CallId::new("a").expect("id")).expect("a");
        fill(&fifo);
        sink.close(&snapshot(&[("x", 1), ("y", 2)]), || {});
        let account = sink.account();
        assert_eq!(account.state, super::StreamState::Sealed);
        assert_eq!(account.drops, 3, "two sealing emissions and the terminal");
        // Once ended, an ordinary record is ignored and counted nowhere.
        super::InstrumentSink::marker(&sink, "LATE");
        assert_eq!(sink.account(), account);
    }

    /// A write that fails outright (no reader left) ends the stream
    /// `failed`, and a terminal record that could not be written is
    /// counted.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_vanished_reader_fails_the_stream() {
        let (_scratch, reader, fifo) = fifo_with_idle_reader();
        let sink = fifo.call(super::CallId::new("a").expect("id")).expect("a");
        drop(reader);
        sink.close(&snapshot(&[("x", 1)]), || {});
        assert_eq!(
            sink.account(),
            super::StreamAccount {
                state: super::StreamState::Failed,
                drops: 2,
            }
        );
    }

    /// A hard failure on the terminal write alone (the reader leaves after
    /// the last sealing emission went out whole) ends the stream `failed`,
    /// with the terminal counted, though the terminal it tried was
    /// `!sealed`.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_hard_failure_on_the_terminal_alone_fails_the_stream() {
        let (_scratch, reader, fifo) = fifo_with_idle_reader();
        let sink = fifo.call(super::CallId::new("a").expect("id")).expect("a");
        sink.close(&snapshot(&[("x", 1), ("y", 2)]), move || drop(reader));
        assert_eq!(
            sink.account(),
            super::StreamAccount {
                state: super::StreamState::Failed,
                drops: 1,
            }
        );
    }

    /// An unwind in the ending (a test hook) is caught: the stream ends
    /// `!failed` with the drops counted before it, and the account agrees
    /// with the terminal record that went out.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_unwind_before_the_terminal_record_ends_the_stream_failed() {
        use std::io::Read as _;
        let (_scratch, mut reader, fifo) = fifo_with_idle_reader();
        let sink = fifo.call(super::CallId::new("a").expect("id")).expect("a");
        sink.close(&snapshot(&[("x", 1)]), || {
            std::panic::resume_unwind(Box::new("hook"));
        });
        assert_eq!(
            sink.account(),
            super::StreamAccount {
                state: super::StreamState::Failed,
                drops: 0,
            }
        );
        // Nonblocking: everything written is read, then `WouldBlock`.
        let mut wire = Vec::new();
        drop(reader.read_to_end(&mut wire));
        let wire = String::from_utf8(wire).expect("utf-8");
        let lines: Vec<&str> = wire
            .lines()
            .map(|l| l.split_once(' ').expect("ts").1)
            .collect();
        assert_eq!(lines, ["%a @@x=1", "%a !failed drops=0"]);
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
