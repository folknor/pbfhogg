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
pub(crate) fn cli_host(cancel: pbfhogg::host::Invocation) -> Host {
    Host {
        diagnostics: pbfhogg::host::Diagnostics::new(Arc::new(StderrSink)),
        process: Arc::new(LegacyProcessActions),
        resources: Arc::new(MachineProbe),
        page_cache: PageCachePolicy::Evict,
        executor: Executor::Ambient,
        invocation: cancel,
    }
}

/// The run's invocation: instrumented with the sidecar marker FIFO when
/// brokkr names one in `BROKKR_MARKER_FIFO`, silent otherwise. The FIFO is
/// returned too, for the CLI's own [`CommandInterval`] markers.
///
/// The library does not read the environment, so the CLI does, opens the FIFO
/// and hands the sink to the process's one invocation. A FIFO that cannot be
/// opened is reported once, here, as a warning: the run continues unprofiled
/// rather than failing.
pub(crate) fn cli_invocation() -> (
    pbfhogg::host::Invocation,
    Option<pbfhogg::debug::MarkerFifo>,
) {
    let Some(path) = std::env::var_os("BROKKR_MARKER_FIFO") else {
        return (pbfhogg::host::Invocation::new(), None);
    };
    let path = std::path::PathBuf::from(path);
    match pbfhogg::debug::MarkerFifo::open(&path) {
        Ok(fifo) => (
            pbfhogg::host::Invocation::with_sink(Arc::new(fifo.clone())),
            Some(fifo),
        ),
        Err(error) => {
            cli_warning(
                "debug.marker_fifo",
                format_args!(
                    "cannot open marker FIFO {}: {error}; sidecar markers are disabled for \
                     this run",
                    path.display()
                ),
            );
            (pbfhogg::host::Invocation::new(), None)
        }
    }
}

/// The reserved process-stream marker opening the CLI's command interval.
pub(crate) const COMMAND_START: &str = "CLI_COMMAND_START";
/// The reserved process-stream marker closing the CLI's command interval.
pub(crate) const COMMAND_END: &str = "CLI_COMMAND_END";

/// The CLI's command interval on the sidecar FIFO's process stream: the
/// untagged pair [`COMMAND_START`] and [`COMMAND_END`], so a harness can set
/// a library call's `entry_us`..`return_us` beside the CLI's own span over
/// the same command instead of the whole process wall.
///
/// `run` opens it once, immediately before dispatching the subcommand, after
/// the command line was parsed and accepted (clap, `validate_command`, the
/// alias refusal and the leftover scan). It closes when the dispatch
/// returns, `Ok` or `Err`, a cancelled run and a refusal raised inside the
/// command (an existing output, a library error) included, and before the
/// end-of-run counters, which follow [`COMMAND_END`] as they follow a
/// harness call's `return_us`. So the interval holds the subcommand's own
/// option resolution, its output and scratch construction, every library
/// call it makes, any publication the CLI does itself, and the summary or
/// report it renders; process policy (the descriptor raise, the arena cap,
/// page-cache eviction) is applied inside it, by the library's hooks.
///
/// A process refused before dispatch, or that prints help or the version,
/// emits neither marker. One that unwinds out of the dispatch emits no
/// [`COMMAND_END`]: the end means the command returned. A forced exit (a
/// second signal, SIGKILL) emits none either, and the FIFO drops a line when
/// it is full, so a reader takes exactly one pair or no interval.
///
/// The two names are the CLI's alone: a `brokkr.toml` text rule keeps them
/// out of the library and every other CLI source.
pub(crate) struct CommandInterval<'a>(Option<&'a pbfhogg::debug::MarkerFifo>);

impl<'a> CommandInterval<'a> {
    /// Emit [`COMMAND_START`]; the returned value emits [`COMMAND_END`] when
    /// it drops. Nothing without a FIFO.
    pub(crate) fn open(fifo: Option<&'a pbfhogg::debug::MarkerFifo>) -> Self {
        use pbfhogg::debug::InstrumentSink as _;
        if let Some(fifo) = fifo {
            fifo.marker(COMMAND_START);
        }
        Self(fifo)
    }
}

impl Drop for CommandInterval<'_> {
    fn drop(&mut self) {
        use pbfhogg::debug::InstrumentSink as _;
        if let Some(fifo) = self.0
            && !std::thread::panicking()
        {
            fifo.marker(COMMAND_END);
        }
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
