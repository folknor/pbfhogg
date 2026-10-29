//! How a failed command is reported and which exit status it earns.

/// Whether `err` is, or wraps, a platform setup refusal. Walks `source()`
/// chains, continuing through foreign errors, and looks inside `io::Error`
/// carriers - including the one inside an `ErrorKind::Io` - because an
/// `io::Error`'s `source()` skips the wrapped error itself; the writers carry
/// the typed refusal inside an `io::Error` to keep their `io::Result`
/// signatures.
pub fn is_platform_unsupported(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = Some(err);
    while let Some(error) = current {
        current = if let Some(error) = error.downcast_ref::<pbfhogg::Error>() {
            match error.kind() {
                pbfhogg::ErrorKind::PlatformUnsupported { .. } => return true,
                pbfhogg::ErrorKind::Io(io) => io
                    .get_ref()
                    .map(|inner| inner as &(dyn std::error::Error + 'static)),
                _ => std::error::Error::source(error),
            }
        } else if let Some(io) = error.downcast_ref::<std::io::Error>() {
            io.get_ref()
                .map(|inner| inner as &(dyn std::error::Error + 'static))
        } else {
            error.source()
        };
    }
    false
}

/// Whether `err` is a cancellation: the run stopped because its token was
/// tripped. Only the authoritative cause counts - the error itself, seen
/// through the transparent carriers this front end and the library put
/// around one (a [`Hinted`] wrapper, an `io::Error` holding a
/// `pbfhogg::Error`) - never a cause further down another error's chain: a
/// failure that merely records a cancellation among its causes is still that
/// failure, and keeps its own exit status.
pub fn is_cancelled(err: &(dyn std::error::Error + 'static)) -> bool {
    if let Some(hinted) = err.downcast_ref::<Hinted>() {
        return is_cancelled(&*hinted.source);
    }
    if let Some(error) = err.downcast_ref::<pbfhogg::Error>() {
        return error.is_cancelled();
    }
    if let Some(io) = err.downcast_ref::<std::io::Error>()
        && let Some(error) = io
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<pbfhogg::Error>())
    {
        return error.is_cancelled();
    }
    false
}

/// A library error with advice that only this front end can give (it names a
/// CLI flag). The typed error stays reachable through `source()` until the
/// final print, so exit-code and chain inspection still see it; only
/// [`error_report`] renders the advice after the cause's text.
#[derive(Debug)]
pub struct Hinted {
    source: Box<dyn std::error::Error + Send + Sync + 'static>,
    suffix: &'static str,
}

impl Hinted {
    pub fn new(
        source: impl std::error::Error + Send + Sync + 'static,
        suffix: &'static str,
    ) -> Self {
        Self {
            source: Box::new(source),
            suffix,
        }
    }
}

impl std::fmt::Display for Hinted {
    /// The advice alone, per the library convention that `Display` never
    /// includes the source's text.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.suffix.trim_start_matches("; "))
    }
}

impl std::error::Error for Hinted {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.source)
    }
}

/// The text printed after `Error: `. A `pbfhogg::Error`, bare or carried
/// inside an `io::Error`, prints its full `report()`; a [`Hinted`] error its
/// cause's text followed by its advice; anything else its own Display.
pub fn error_report(err: &(dyn std::error::Error + 'static)) -> String {
    if let Some(hinted) = err.downcast_ref::<Hinted>() {
        return format!("{}{}", error_report(&*hinted.source), hinted.suffix);
    }
    if let Some(error) = err.downcast_ref::<pbfhogg::Error>() {
        return error.report();
    }
    if let Some(io) = err.downcast_ref::<std::io::Error>()
        && let Some(error) = io
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<pbfhogg::Error>())
    {
        return error.report();
    }
    err.to_string()
}

/// What the invoked command line can be advised to do after a typed library
/// refusal: each field is the switch this command and mode actually offer
/// (and do not already have set), or nothing. Computed by `run` from the
/// parsed command and its matches; the library names no flag and no command.
#[derive(Clone, Copy, Debug, Default)]
pub struct Advice {
    /// What the command accepts for unindexed input (`--force`,
    /// `--allow-unindexed`), printed after the indexing recipe.
    pub indexdata: Option<&'static str>,
    /// The element-cap flag that shrinks an oversized blob
    /// (`--elements-per-blob`, `--block-cap`).
    pub element_cap: Option<&'static str>,
    /// Whether `--allow-version-regression` is on offer.
    pub allow_version_regression: bool,
    /// Whether `--ignore-missing-nodes` is on offer.
    pub ignore_missing_nodes: bool,
    /// Whether the run maintains locations through a merge
    /// (`apply-changes --locations-on-ways`), where unresolved node
    /// locations most often mean a base that lost its untagged nodes.
    pub merge_locations: bool,
}

/// What `cat --reindex` does for a file with false indexdata, and when it
/// is the repair. Shared by the error hint and `check`'s stopped note.
pub(crate) const REINDEX_ADVICE: &str = "rewrites the indexdata from the blobs' payloads, which repairs it when every data block \
     can be described by one index";

/// `path` as a shell word: bare when it is plain, single-quoted otherwise.
pub(crate) fn shell_word(path: &std::path::Path) -> String {
    let text = path.display().to_string();
    let plain = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | '+' | ':'));
    if plain {
        text
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

/// The input an error names, as a shell word, or a placeholder.
fn input_word(error: &pbfhogg::Error, placeholder: &str) -> String {
    error
        .source_path()
        .map_or_else(|| placeholder.to_owned(), shell_word)
}

/// The advice for one typed error, if this front end has any for it.
fn hint_for(error: &pbfhogg::Error, advice: &Advice) -> Option<String> {
    use pbfhogg::ErrorKind;
    match error.kind() {
        // A partially indexed file has no fallback to grant, so the escape
        // switch is not advice there.
        ErrorKind::MissingIndexdata { partial, .. } => Some(format!(
            "\n\nGenerate an indexed PBF first:\n\n  pbfhogg cat {} -o indexed.osm.pbf{}",
            input_word(error, "input.osm.pbf"),
            if *partial {
                ""
            } else {
                advice.indexdata.unwrap_or("")
            }
        )),
        // One remedy for every Unsorted: a header without the sorted claim
        // sorts the same either way, and one whose claim the file
        // contradicts is copied unchanged by plain `sort`, which believes
        // the claim and the blobs' indexdata.
        ErrorKind::Unsorted { .. } => Some(format!(
            "\n\nSort the input first, decoding every blob rather than believing the header's \
             sort claim and the blobs' indexdata:\n\n  pbfhogg sort --distrust-metadata {} -o \
             sorted.osm.pbf",
            input_word(error, "input.osm.pbf")
        )),
        ErrorKind::IndexdataMismatch { .. } => {
            let input = input_word(error, "input.osm.pbf");
            Some(format!(
                "; `pbfhogg check --index {input}` reports which blobs disagree with their \
                 contents, and `pbfhogg cat --reindex {input} -o reindexed.osm.pbf` {REINDEX_ADVICE}"
            ))
        }
        ErrorKind::MissingNodeLocations { .. }
            if advice.merge_locations || advice.ignore_missing_nodes =>
        {
            let mut hint = String::new();
            if advice.merge_locations {
                hint.push_str(
                    "; a base made by add-locations-to-ways must keep its untagged nodes \
                     (--keep-untagged-nodes) for the merge to resolve them",
                );
            }
            if advice.ignore_missing_nodes {
                hint.push_str("; pass --ignore-missing-nodes to write them as (0, 0)");
            }
            Some(hint)
        }
        ErrorKind::BlobTooLarge { .. } => advice.element_cap.map(|flag| format!("; lower {flag}")),
        ErrorKind::VersionRegression { .. } if advice.allow_version_regression => {
            Some("; pass --allow-version-regression to permit lower-version changes".to_owned())
        }
        ErrorKind::Output(pbfhogg::OutputError::Exists { .. }) => {
            Some("; pass --overwrite (-O) to replace it".to_owned())
        }
        ErrorKind::Output(pbfhogg::OutputError::ExclusivePublishUnsupported { .. }) => {
            Some("; pass --overwrite (-O) to publish without the exclusive check".to_owned())
        }
        ErrorKind::Output(pbfhogg::OutputError::StagingDir { .. }) => Some(
            "; choose a --staging-dir on the same mount as the output that neither is nor \
             contains the output's directory"
                .to_owned(),
        ),
        _ => None,
    }
}

/// Advice for a typed refusal, naming the flag or command that addresses it.
/// The library error names neither; this front end does. Walks the chain the
/// way [`is_platform_unsupported`] does, so a refusal raised deep inside a
/// command (at publication, inside a worker) is found too, and a path in the
/// advice is the one the matching error names.
pub fn output_hint(err: &(dyn std::error::Error + 'static), advice: &Advice) -> Option<String> {
    let mut current = Some(err);
    while let Some(error) = current {
        current = if let Some(error) = error.downcast_ref::<pbfhogg::Error>() {
            if let Some(hint) = hint_for(error, advice) {
                return Some(hint);
            }
            match error.kind() {
                pbfhogg::ErrorKind::Io(io) => io
                    .get_ref()
                    .map(|inner| inner as &(dyn std::error::Error + 'static)),
                _ => std::error::Error::source(error),
            }
        } else if let Some(io) = error.downcast_ref::<std::io::Error>() {
            io.get_ref()
                .map(|inner| inner as &(dyn std::error::Error + 'static))
        } else {
            error.source()
        };
    }
    None
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // fixture files in test scratch, not operator input
mod tests {
    use super::{Hinted, error_report, is_platform_unsupported};
    use crate::test_scratch::scratch;
    use std::io;

    fn platform() -> pbfhogg::Error {
        pbfhogg::Error::platform_unsupported("io-uring", io::Error::other("refused"))
    }

    #[test]
    fn bare_and_context_wrapped_refusals_exit_3() {
        assert!(is_platform_unsupported(&platform()));
        assert!(is_platform_unsupported(&platform().context("opening")));
    }

    #[test]
    fn io_carrier_and_double_wrapping_exit_3() {
        let carried = io::Error::from(platform());
        assert!(is_platform_unsupported(&carried));
        let once = pbfhogg::Error::from(carried);
        assert!(is_platform_unsupported(&once));
        // An Io error holding an io::Error that holds the refusal, which a
        // plain source() walk skips.
        let twice = pbfhogg::Error::from(io::Error::other(io::Error::from(platform())));
        assert!(is_platform_unsupported(&twice));
    }

    #[test]
    fn refusal_under_external_exits_3() {
        let external = pbfhogg::Error::external(platform());
        assert!(is_platform_unsupported(&external));
        let boxed: Box<dyn std::error::Error> = Box::new(external);
        assert!(is_platform_unsupported(boxed.as_ref()));
    }

    #[test]
    fn ordinary_errors_do_not() {
        assert!(!is_platform_unsupported(&pbfhogg::Error::from(
            io::Error::other("x")
        )));
        assert!(!is_platform_unsupported(&pbfhogg::Error::external(
            io::Error::other("x")
        )));
        assert!(!is_platform_unsupported(&io::Error::other("x")));
    }

    #[test]
    fn report_covers_bare_carried_and_foreign_errors() {
        assert_eq!(
            error_report(&platform()),
            "platform feature io-uring unavailable: refused"
        );
        let carried = io::Error::from(platform().context("opening"));
        assert_eq!(
            error_report(&carried),
            "opening: platform feature io-uring unavailable: refused"
        );
        assert_eq!(error_report(&io::Error::other("plain")), "plain");
    }

    #[test]
    fn hinted_error_keeps_the_typed_cause_reachable_and_prints_cause_then_advice() {
        let hinted = Hinted::new(platform().context("opening"), "; try again");
        // The advice never hides the refusal from the exit-code walk.
        assert!(is_platform_unsupported(&hinted));
        assert_eq!(
            error_report(&hinted),
            "opening: platform feature io-uring unavailable: refused; try again"
        );
        assert_eq!(hinted.to_string(), "try again");
    }

    /// An existing output earns the advice naming the flag that grants the
    /// replacement, however deep in the chain the refusal sits; nothing
    /// else does.
    #[test]
    fn output_refusal_earns_the_overwrite_advice() {
        use super::output_hint;
        let dir = scratch();
        let existing = dir.path().join("out.osm.pbf");
        std::fs::write(&existing, b"user data").expect("write existing output");
        let refused = pbfhogg::Output::new(
            &existing,
            pbfhogg::Overwrite::Refuse,
            &pbfhogg::host::Diagnostics::discard(),
        )
        .expect_err("an existing output is refused");
        assert!(matches!(
            refused.kind(),
            pbfhogg::ErrorKind::Output(pbfhogg::OutputError::Exists { .. })
        ));
        let advice = super::Advice::default();
        assert_eq!(
            output_hint(&refused, &advice).as_deref(),
            Some("; pass --overwrite (-O) to replace it")
        );
        let carried = io::Error::from(refused.context("publishing"));
        assert!(output_hint(&carried, &advice).is_some());
        assert_eq!(output_hint(&platform(), &advice), None);
    }

    /// A cancellation earns the interrupted status as the error itself or
    /// through the transparent carriers, never as a cause buried under
    /// another error.
    #[test]
    fn cancellation_is_classified_only_as_the_authoritative_cause() {
        use super::is_cancelled;
        let dir = scratch();
        let output = pbfhogg::Output::new(
            dir.path().join("out.osm.pbf"),
            pbfhogg::Overwrite::Refuse,
            &pbfhogg::host::Diagnostics::discard(),
        )
        .expect("fresh output");
        let cancel = pbfhogg::host::Invocation::new();
        let output = output.bind(&cancel).expect("open");
        let _ = cancel.cancel();
        let cancelled = output
            .publish()
            .expect_err("a tripped token refuses publication");
        assert!(cancelled.is_cancelled());
        assert!(is_cancelled(&cancelled));
        assert!(is_cancelled(&cancelled.clone().context("stage 3")));
        assert!(is_cancelled(&io::Error::from(cancelled.clone())));
        assert!(is_cancelled(&Hinted::new(cancelled.clone(), "; advice")));
        let boxed: Box<dyn std::error::Error> = Box::new(cancelled.clone());
        assert!(is_cancelled(boxed.as_ref()));
        // Buried as the cause of another failure: that failure stands.
        assert!(!is_cancelled(&pbfhogg::Error::external(cancelled)));
        assert!(!is_cancelled(&platform()));
        assert!(!is_platform_unsupported(&pbfhogg::Error::from(
            io::Error::other("x")
        )));
    }
}
