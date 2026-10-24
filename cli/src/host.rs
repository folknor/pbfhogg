use pbfhogg::host::{
    Event, EventKind, Executor, Host, MachineProbe, PageCachePolicy, ProcessActions,
};
use pbfhogg::process::FdAllowance;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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

/// Set once, right after the command line is parsed, by `-q, --quiet`.
/// Process state of this binary: the library never sees it (ADR-0016).
static QUIET: AtomicBool = AtomicBool::new(false);

/// Record `-q, --quiet` for the rest of the run.
pub(crate) fn set_quiet(quiet: bool) {
    QUIET.store(quiet, Ordering::Relaxed);
}

/// Whether an event of `kind` reaches stderr. `-q, --quiet` means "the exit
/// code is the answer": what stays is what explains that code, the warnings
/// (the `Error:` line never passes through here). Notes, progress, summaries
/// and developer lines go. A kind added later is dropped under `--quiet`
/// until it is placed here.
fn prints(kind: EventKind, quiet: bool) -> bool {
    !quiet || matches!(kind, EventKind::Warning)
}

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

/// Advice this front end appends to a library diagnostic, keyed on the
/// diagnostic's key. The library's text names no flag and no command; the
/// switch or command that acts on what it reports is named here.
fn diagnostic_hint(key: &str) -> &'static str {
    match key {
        "altw.external_hint" => {
            " Pass --index-type external to use it, or --index-type auto to choose."
        }
        "extract.passthrough_demoted" => " `pbfhogg check --index` checks it.",
        "check.ids.unindexed_triple_scan" => {
            " `pbfhogg cat input.osm.pbf -o indexed.osm.pbf` writes an indexed copy."
        }
        _ => "",
    }
}

impl pbfhogg::host::DiagnosticSink for StderrSink {
    /// The one place a diagnostic becomes stderr text. Library text carries no
    /// kind prefix of its own, so the kind alone decides the rendering, and
    /// whether it is rendered at all under `--quiet`; a `--json` sink
    /// (serializing kind, key and the formatted arguments) belongs here too
    /// and nowhere in the library.
    fn event(&self, event: &pbfhogg::host::Event<'_>) {
        if !prints(event.kind, QUIET.load(Ordering::Relaxed)) {
            return;
        }
        let hint = diagnostic_hint(event.key);
        match diagnostic_prefix(event.kind) {
            Some(prefix) => {
                crate::output::stderr_line(format_args!("{prefix}{}{hint}", event.args));
            }
            None => crate::output::stderr_line(format_args!("{}{hint}", event.args)),
        }
    }
}

/// Report a note the CLI itself raises, through the same sink and prefix as
/// the library's notes.
pub(crate) fn cli_note(key: &'static str, args: std::fmt::Arguments<'_>) {
    use pbfhogg::host::DiagnosticSink as _;
    StderrSink.event(&Event {
        kind: EventKind::Note,
        key,
        args,
    });
}

/// Report progress the CLI itself prints (a run's opening line), through the
/// same sink as the library's progress, so `--quiet` drops it with theirs.
pub(crate) fn cli_progress(key: &'static str, args: std::fmt::Arguments<'_>) {
    use pbfhogg::host::DiagnosticSink as _;
    StderrSink.event(&Event {
        kind: EventKind::Progress,
        key,
        args,
    });
}

/// The pointer a summary that counted elements from unverified indexdata
/// claims gets: the library reports the claim, this front end names the
/// command that checks it.
pub(crate) fn note_unverified_claims(claimed: u64) {
    if claimed > 0 {
        cli_note(
            "cli.unverified_indexdata",
            format_args!(
                "counts marked unverified come from blob indexdata; `pbfhogg check --index` \
                 checks it"
            ),
        );
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

#[cfg(test)]
mod tests {
    use super::prints;
    use pbfhogg::host::EventKind;

    /// `--quiet` keeps the warnings and nothing else; without it every kind
    /// prints.
    #[test]
    fn quiet_keeps_only_warnings() {
        for kind in [
            EventKind::Progress,
            EventKind::Warning,
            EventKind::Note,
            EventKind::Summary,
            EventKind::Developer,
        ] {
            assert!(prints(kind, false), "{kind:?} must print without --quiet");
            assert_eq!(
                prints(kind, true),
                kind == EventKind::Warning,
                "{kind:?} under --quiet"
            );
        }
    }
}
