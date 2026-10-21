//! Caller-owned process actions and resources for a library invocation.
pub use crate::process::FdAllowance;
use std::{num::NonZeroUsize, sync::Arc};

/// Explicit facilities supplied by the caller. There is deliberately no default.
#[derive(Clone)]
pub struct Host {
    pub diagnostics: Diagnostics,
    pub process: Arc<dyn ProcessActions>,
    pub resources: Arc<dyn Resources>,
}
/// Invocation count and position are part of the contract, never deduplicated or hoisted.
pub trait ProcessActions: Send + Sync {
    /// A pool will allocate and free buffers on different threads.
    fn cross_thread_alloc_pool(&self);
    /// Request permission to hold `wanted` descriptors.
    fn fd_allowance(&self, wanted: u64) -> FdAllowance;
}
/// Leave process policy unchanged.
pub struct NoProcessActions;
impl ProcessActions for NoProcessActions {
    fn cross_thread_alloc_pool(&self) {}
    fn fd_allowance(&self, _wanted: u64) -> FdAllowance {
        crate::process::current_nofile()
    }
}
/// Per-call resource probes, invoked where the library needs the value.
pub trait Resources: Send + Sync {
    fn parallelism(&self) -> Option<NonZeroUsize>;
    fn memory_available(&self) -> Option<u64>;
}
/// Probe the current machine.
pub struct MachineProbe;
impl Resources for MachineProbe {
    fn parallelism(&self) -> Option<NonZeroUsize> {
        std::thread::available_parallelism().ok()
    }
    fn memory_available(&self) -> Option<u64> {
        let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
        let line = meminfo.lines().find(|l| l.starts_with("MemAvailable:"))?;
        let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some(kb.saturating_mul(1024))
    }
}
/// Fixed resource values for isolated or reproducible runs.
pub struct Fixed {
    pub parallelism: Option<NonZeroUsize>,
    pub memory_available: Option<u64>,
}
impl Resources for Fixed {
    fn parallelism(&self) -> Option<NonZeroUsize> {
        self.parallelism
    }
    fn memory_available(&self) -> Option<u64> {
        self.memory_available
    }
}
pub(crate) fn sized_workers(
    r: &dyn Resources,
    reserve: usize,
    min: usize,
    fallback: usize,
) -> usize {
    r.parallelism()
        .map(|n| n.get().saturating_sub(reserve).max(min))
        .unwrap_or(fallback)
}
#[derive(Clone)]
pub(crate) struct ResourcesHandle(pub(crate) Arc<dyn Resources>);
impl std::fmt::Debug for ResourcesHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ResourcesHandle")
    }
}
#[cfg(test)]
impl Host {
    pub(crate) fn for_tests() -> Self {
        Self {
            diagnostics: Diagnostics::discard(),
            process: Arc::new(NoProcessActions),
            resources: Arc::new(MachineProbe),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn worker_sizing_respects_fixed_resources() {
        for (reserve, min, fallback) in [(1, 1, 3), (2, 1, 4), (2, 2, 4), (0, 1, 8), (0, 1, 4)] {
            for n in [None, Some(1), Some(2), Some(16)] {
                let fixed = Fixed {
                    parallelism: n.and_then(NonZeroUsize::new),
                    memory_available: None,
                };
                assert_eq!(
                    sized_workers(&fixed, reserve, min, fallback),
                    n.map_or(fallback, |v| v.saturating_sub(reserve).max(min))
                );
            }
        }
    }
    #[test]
    fn descriptor_allowance_narration() {
        use crate::process::FdAdjustment::*;
        for (adjustment, text) in [
            (None, "RLIMIT_NOFILE: 4096 (not adjusted)"),
            (
                Some(ProbeFailed),
                "RLIMIT_NOFILE: probe failed; assuming 4096",
            ),
            (
                Some(AlreadyAtCap),
                "RLIMIT_NOFILE: 4096 (already at hard cap; no adjustment)",
            ),
            (
                Some(Raised { from: 1024 }),
                "RLIMIT_NOFILE: 1024 -> 4096 (soft raised to hard cap)",
            ),
            (
                Some(RaiseFailed),
                "RLIMIT_NOFILE: 4096 (raise to hard cap rejected; still at soft cap)",
            ),
        ] {
            assert_eq!(
                FdAllowance {
                    descriptors: 4096,
                    adjustment
                }
                .to_string(),
                text
            );
        }
    }
}

/// A diagnostic event without a line terminator.
pub struct Event<'a> {
    pub kind: EventKind,
    pub key: &'static str,
    pub args: std::fmt::Arguments<'a>,
}
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    Progress,
    Warning,
    Note,
    Summary,
    Developer,
}
/// A sink receives one event per line and may serve concurrent callers.
pub trait DiagnosticSink: Send + Sync {
    fn event(&self, event: &Event<'_>);
}
#[derive(Default)]
pub struct OnceKeys {
    keys: std::sync::Mutex<std::collections::HashSet<&'static str>>,
}
#[derive(Clone)]
pub struct Diagnostics {
    sink: Arc<dyn DiagnosticSink>,
    once: Arc<OnceKeys>,
}
impl std::fmt::Debug for Diagnostics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Diagnostics")
    }
}
struct Discard;
impl DiagnosticSink for Discard {
    fn event(&self, _event: &Event<'_>) {}
}
impl Diagnostics {
    pub fn new(sink: Arc<dyn DiagnosticSink>) -> Self {
        Self::with_once(sink, Arc::new(OnceKeys::default()))
    }
    pub fn with_once(sink: Arc<dyn DiagnosticSink>, once: Arc<OnceKeys>) -> Self {
        Self { sink, once }
    }
    pub fn discard() -> Self {
        Self::new(Arc::new(Discard))
    }
    pub fn summary_writer(&self, key: &'static str) -> SummaryWriter<'_> {
        SummaryWriter {
            diagnostics: self,
            key,
            bytes: Vec::new(),
        }
    }
    pub(crate) fn emit(&self, kind: EventKind, key: &'static str, args: std::fmt::Arguments<'_>) {
        self.sink.event(&Event { kind, key, args });
    }
    pub(crate) fn emit_once(
        &self,
        kind: EventKind,
        key: &'static str,
        args: std::fmt::Arguments<'_>,
    ) {
        let first = self
            .once
            .keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key);
        if first {
            self.emit(kind, key, args);
        }
    }
}
/// Converts byte-oriented summary output into one event per completed line.
pub struct SummaryWriter<'a> {
    diagnostics: &'a Diagnostics,
    key: &'static str,
    bytes: Vec<u8>,
}
impl std::io::Write for SummaryWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.bytes.extend_from_slice(buf);
        while let Some(end) = self.bytes.iter().position(|b| *b == b'\n') {
            self.diagnostics.emit(
                EventKind::Summary,
                self.key,
                format_args!("{}", String::from_utf8_lossy(&self.bytes[..end])),
            );
            self.bytes.drain(..=end);
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Drop for SummaryWriter<'_> {
    fn drop(&mut self) {
        if !self.bytes.is_empty() {
            self.diagnostics.emit(
                EventKind::Summary,
                self.key,
                format_args!("{}", String::from_utf8_lossy(&self.bytes)),
            );
        }
    }
}
macro_rules! diag {
    ($d:expr, $kind:ident, $key:literal, $($args:tt)+) => { $d.emit($crate::host::EventKind::$kind, $key, format_args!($($args)+)) };
}
macro_rules! diag_once {
    ($d:expr, $kind:ident, $key:literal, $($args:tt)+) => { $d.emit_once($crate::host::EventKind::$kind, $key, format_args!($($args)+)) };
}
pub(crate) use {diag, diag_once};

#[cfg(test)]
mod diagnostic_tests {
    use super::*;
    use std::io::Write;
    #[derive(Default)]
    struct Sink(std::sync::Mutex<Vec<String>>);
    impl DiagnosticSink for Sink {
        fn event(&self, e: &Event<'_>) {
            self.0.lock().expect("sink").push(e.args.to_string());
        }
    }
    #[test]
    fn once_keys_share_only_when_requested() {
        let sink = Arc::new(Sink::default());
        let once = Arc::new(OnceKeys::default());
        let first = Diagnostics::with_once(
            Arc::clone(&sink) as Arc<dyn DiagnosticSink>,
            Arc::clone(&once),
        );
        let shared = Diagnostics::with_once(Arc::clone(&sink) as Arc<dyn DiagnosticSink>, once);
        let separate = Diagnostics::new(Arc::clone(&sink) as Arc<dyn DiagnosticSink>);
        for d in [first, shared, separate] {
            d.emit_once(EventKind::Warning, "key", format_args!("warning"));
        }
        assert_eq!(*sink.0.lock().expect("sink"), ["warning", "warning"]);
    }
    #[test]
    fn summary_writer_splits_lines_and_keeps_partial_on_drop() {
        let sink = Arc::new(Sink::default());
        let d = Diagnostics::new(Arc::clone(&sink) as Arc<dyn DiagnosticSink>);
        {
            let mut w = d.summary_writer("summary");
            writeln!(w, "one").expect("write");
            writeln!(w, "two").expect("write");
            write!(w, "par").expect("write");
            writeln!(w, "tial").expect("write");
            write!(w, "tail").expect("write");
        }
        assert_eq!(
            *sink.0.lock().expect("sink"),
            ["one", "two", "partial", "tail"]
        );
    }
}
