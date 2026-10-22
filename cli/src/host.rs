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

pub(crate) struct StderrSink;
impl pbfhogg::host::DiagnosticSink for StderrSink {
    fn event(&self, event: &pbfhogg::host::Event<'_>) {
        crate::output::stderr_line(event.args);
    }
}
