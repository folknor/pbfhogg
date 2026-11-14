//! The host the instruments run under, chosen here because the library
//! chooses nothing about the process (ADR-0016). It matches the CLI's: the
//! machine's resources, page-cache eviction behind the read head, rayon's
//! ambient pool, the descriptor limit raised to its hard cap for the run, and
//! the sidecar marker FIFO when brokkr names one.

use pbfhogg::host::{
    Diagnostics, Event, EventKind, Executor, Host, Invocation, MachineProbe, PageCachePolicy,
    ProcessActions,
};
use pbfhogg::process::FdReservation;
use std::sync::Arc;

/// One run per process, so the raise stands for its lifetime and nothing
/// restores it.
struct RaiseNofile;

impl ProcessActions for RaiseNofile {
    fn fd_allowance(&self, _wanted: u64) -> FdReservation {
        FdReservation::unreserved(pbfhogg::process::raise_nofile_to_hard_cap())
    }
}

/// Diagnostics as stderr lines, warnings and notes prefixed. Instrument
/// output is the key=value block the bench scripts parse; a diagnostic line
/// never has that shape, since every one carries prose.
struct StderrSink;

impl pbfhogg::host::DiagnosticSink for StderrSink {
    fn event(&self, event: &Event<'_>) {
        let prefix = match event.kind {
            EventKind::Warning => "Warning: ",
            EventKind::Note => "Note: ",
            _ => "",
        };
        eprintln!("{prefix}{}", event.args);
    }
}

fn warning(key: &'static str, args: std::fmt::Arguments<'_>) {
    use pbfhogg::host::DiagnosticSink as _;
    StderrSink.event(&Event {
        kind: EventKind::Warning,
        key,
        args,
    });
}

/// The run's invocation: carrying the marker FIFO's sink when brokkr names
/// one in `BROKKR_MARKER_FIFO`, plain otherwise. The FIFO is returned too,
/// for the instruments' own phase markers. A FIFO that cannot be opened
/// costs this run its markers and counters, not its result, and not its
/// hotpath report, which goes to `HOTPATH_OUTPUT_PATH` independently.
pub(crate) fn invocation() -> (Invocation, Option<pbfhogg::debug::MarkerFifo>) {
    let Some(path) = std::env::var_os("BROKKR_MARKER_FIFO") else {
        return (Invocation::new(), None);
    };
    let path = std::path::PathBuf::from(path);
    match pbfhogg::debug::MarkerFifo::open(&path) {
        Ok(fifo) => (Invocation::with_sink(Arc::new(fifo.clone())), Some(fifo)),
        Err(error) => {
            warning(
                "bench.marker_fifo",
                format_args!(
                    "cannot open marker FIFO {}: {error}; this run records no markers or counters",
                    path.display()
                ),
            );
            (Invocation::new(), None)
        }
    }
}

/// An instrument's own phase markers, on the process stream of the FIFO
/// (`FOO_START` / `FOO_END` pairs, `brokkr man measure`); nothing without a
/// FIFO.
#[derive(Clone, Copy)]
pub(crate) struct Markers<'a>(pub(crate) Option<&'a pbfhogg::debug::MarkerFifo>);

impl Markers<'_> {
    pub(crate) fn marker(self, name: &str) {
        use pbfhogg::debug::InstrumentSink as _;
        if let Some(fifo) = self.0 {
            fifo.marker(name);
        }
    }
}

/// The host for `invocation`.
pub(crate) fn host(invocation: Invocation) -> Host {
    Host {
        diagnostics: Diagnostics::new(Arc::new(StderrSink)),
        process: Arc::new(RaiseNofile),
        resources: Arc::new(MachineProbe),
        page_cache: PageCachePolicy::Evict,
        executor: Executor::Ambient,
        invocation,
    }
}
