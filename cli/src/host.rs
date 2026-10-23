use pbfhogg::host::{
    Event, EventKind, Executor, Host, MachineProbe, PageCachePolicy, ProcessActions,
};
use pbfhogg::process::FdAllowance;
use std::sync::Arc;
pub(crate) struct LegacyProcessActions;
impl ProcessActions for LegacyProcessActions {
    fn cross_thread_alloc_pool(&self) {
        pbfhogg::process::cap_malloc_arenas();
    }
    fn fd_allowance(&self, _wanted: u64) -> FdAllowance {
        // CLI process policy: keep the raise for this command's lifetime.
        // Embedded callers choose their own ProcessActions; the library must
        // not restore this limit and race other process users (ADR-0016).
        pbfhogg::process::raise_nofile_to_hard_cap()
    }
}
/// The CLI's host. `cancel` is the token the signal waiter trips (see
/// `signals`).
pub(crate) fn cli_host(cancel: pbfhogg::host::Cancel) -> Host {
    Host {
        diagnostics: pbfhogg::host::Diagnostics::new(Arc::new(StderrSink)),
        process: Arc::new(LegacyProcessActions),
        resources: Arc::new(MachineProbe),
        page_cache: PageCachePolicy::Evict,
        executor: Executor::Ambient,
        cancel,
    }
}

/// Arm the sidecar marker FIFO when brokkr names one in `BROKKR_MARKER_FIFO`.
///
/// The library does not read the environment, so the CLI does and hands the
/// path over. A FIFO that cannot be opened is reported once, here, as a
/// warning: the run continues unprofiled rather than failing.
pub(crate) fn init_marker_fifo() {
    let Some(path) = std::env::var_os("BROKKR_MARKER_FIFO") else {
        return;
    };
    let path = std::path::PathBuf::from(path);
    if let Err(error) = pbfhogg::debug::initialize_marker_fifo(&path) {
        cli_warning(
            "debug.marker_fifo",
            format_args!(
                "cannot open marker FIFO {}: {error}; sidecar markers are disabled for this run",
                path.display()
            ),
        );
    }
}

pub(crate) struct StderrSink;

/// The one CLI prefix for each warning-class event kind.
///
/// Only warnings and notes are prefixed, so an operator can grep one spelling
/// per class. Progress, summary and developer events are routine output and
/// print as they always have: prefixing them would change every ordinary
/// stderr line and drop the command names some summaries lead with. `None`
/// prints the event as is.
fn diagnostic_prefix(kind: EventKind) -> Option<&'static str> {
    match kind {
        EventKind::Warning => Some("Warning: "),
        EventKind::Note => Some("Note: "),
        _ => None,
    }
}

impl pbfhogg::host::DiagnosticSink for StderrSink {
    /// The one place a diagnostic becomes stderr text. Library text carries no
    /// kind prefix of its own, so the kind alone decides the rendering; a
    /// `--quiet` kind filter or a `--json` sink (serializing kind, key and the
    /// formatted arguments) belongs here and nowhere in the library.
    fn event(&self, event: &pbfhogg::host::Event<'_>) {
        match diagnostic_prefix(event.kind) {
            Some(prefix) => crate::output::stderr_line(format_args!("{prefix}{}", event.args)),
            None => crate::output::stderr_line(event.args),
        }
    }
}

/// Report a warning the CLI itself raises (a degraded environment, a legacy
/// flag that grants more than it says), through the same sink and prefix as
/// the library's events. An option that would have no effect is refused, not
/// warned about.
pub(crate) fn cli_warning(key: &'static str, args: std::fmt::Arguments<'_>) {
    use pbfhogg::host::DiagnosticSink as _;
    StderrSink.event(&Event {
        kind: EventKind::Warning,
        key,
        args,
    });
}
