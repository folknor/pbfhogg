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
                "cannot open marker FIFO {}: {error}; sidecar markers are disabled for this run",
                path.display()
            ),
        });
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

/// The body prefix a library warning or note still carries, keyed by event.
///
/// Library event text stays as it is for other sinks, so the CLI removes the
/// old per-site prefix before adding its own. Leading indentation (a warning
/// nested under a progress heading) is trimmed first, then this prefix. A new
/// warning or note whose text opens with its own prefix needs an entry here,
/// or it prints doubled.
fn legacy_prefix(event: &Event<'_>) -> Option<&'static str> {
    match (event.kind, event.key) {
        (
            EventKind::Warning,
            "header.way_metadata_loss"
            | "sort.unindexed_fallback"
            | "repack.way_metadata_loss"
            | "repack.cap_never_fired"
            | "extract.passthrough_demoted"
            | "writer.tagdata_dropped_oversize",
        ) => Some("Warning: "),
        (
            EventKind::Warning,
            "indexdata.unrecognised_length" | "check.ids.unindexed_triple_scan",
        ) => Some("warning: "),
        (EventKind::Warning, "geocode.pass2.admin_ring_budget") => Some("WARNING: "),
        (EventKind::Note, "altw.external_hint") => Some("hint: "),
        (EventKind::Note, "tags_count.blobs_skipped_by_tag") => Some("[tags-count] "),
        (
            EventKind::Note,
            "tags_filter.blobs_skipped_by_tag" | "tags_filter.pass2_blobs_skipped",
        ) => Some("[tags-filter] "),
        (EventKind::Note, "getid.invert.passthrough") => Some("[getid --invert] "),
        (EventKind::Note, "getid.blobs_skipped") => Some("[getid] "),
        (EventKind::Note, "inspect.direct_io_ignored") => Some("[inspect] "),
        _ => None,
    }
}

impl pbfhogg::host::DiagnosticSink for StderrSink {
    fn event(&self, event: &pbfhogg::host::Event<'_>) {
        let Some(prefix) = diagnostic_prefix(event.kind) else {
            crate::output::stderr_line(event.args);
            return;
        };
        // Warnings and notes are rare, so formatting them to a String first
        // costs nothing that matters.
        let message = event.args.to_string();
        let body = message.trim_start_matches(' ');
        let body = legacy_prefix(event)
            .and_then(|old| body.strip_prefix(old))
            .unwrap_or(body);
        crate::output::stderr_line(format_args!("{prefix}{body}"));
    }
}
