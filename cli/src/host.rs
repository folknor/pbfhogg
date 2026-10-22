use pbfhogg::host::{Executor, Host, MachineProbe, PageCachePolicy, ProcessActions};
use pbfhogg::process::FdAllowance;
use std::sync::Arc;
pub(crate) struct LegacyProcessActions;
impl ProcessActions for LegacyProcessActions {
    fn cross_thread_alloc_pool(&self) {
        pbfhogg::process::cap_malloc_arenas();
    }
    fn fd_allowance(&self, _wanted: u64) -> FdAllowance {
        pbfhogg::process::raise_nofile_to_hard_cap()
    }
}
pub(crate) fn cli_host() -> Host {
    Host {
        diagnostics: pbfhogg::host::Diagnostics::new(Arc::new(StderrSink)),
        process: Arc::new(LegacyProcessActions),
        resources: Arc::new(MachineProbe),
        page_cache: PageCachePolicy::Evict,
        executor: Executor::Ambient,
        scratch_parent: None,
    }
}

/// Arm the sidecar marker FIFO when brokkr names one in `BROKKR_MARKER_FIFO`.
///
/// The library does not read the environment, so the CLI does and hands the
/// path over. A FIFO that cannot be opened is reported once, here, as a
/// warning: the run continues unprofiled rather than failing.
pub(crate) fn init_marker_fifo() {
    use pbfhogg::host::DiagnosticSink as _;
    let Some(path) = std::env::var_os("BROKKR_MARKER_FIFO") else {
        return;
    };
    let path = std::path::PathBuf::from(path);
    if let Err(error) = pbfhogg::debug::initialize_marker_fifo(&path) {
        StderrSink.event(&pbfhogg::host::Event {
            kind: pbfhogg::host::EventKind::Warning,
            key: "debug.marker_fifo",
            args: format_args!(
                "Warning: cannot open marker FIFO {}: {error}; sidecar markers are disabled for this run",
                path.display()
            ),
        });
    }
}

pub(crate) struct StderrSink;
impl pbfhogg::host::DiagnosticSink for StderrSink {
    fn event(&self, event: &pbfhogg::host::Event<'_>) {
        crate::output::stderr_line(event.args);
    }
}
