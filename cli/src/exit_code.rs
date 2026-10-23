//! How a failed command is reported and which exit status it earns. Kept in
//! a file of its own, free of the rest of the binary, so the CLI integration
//! tests can compile it directly (`#[path]`) and test the error walk without
//! a unit-test harness in `src/`.

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
/// (and do not already have set), or nothing. Computed by `main` from the
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
}

/// `path` as a shell word: bare when it is plain, single-quoted otherwise.
fn shell_word(path: &std::path::Path) -> String {
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
        ErrorKind::MissingIndexdata { .. } => Some(format!(
            "\n\nGenerate an indexed PBF first:\n\n  pbfhogg cat {} -o indexed.osm.pbf{}",
            input_word(error, "input.osm.pbf"),
            advice.indexdata.unwrap_or("")
        )),
        ErrorKind::Unsorted { .. } => Some(format!(
            "\n\nSort the input first:\n\n  pbfhogg sort {} -o sorted.osm.pbf",
            input_word(error, "input.osm.pbf")
        )),
        ErrorKind::IndexdataMismatch { .. } => Some(format!(
            "; `pbfhogg check --index {}` reports which blobs disagree with their contents",
            input_word(error, "input.osm.pbf")
        )),
        ErrorKind::MissingNodeLocations { .. } if advice.ignore_missing_nodes => {
            Some("; pass --ignore-missing-nodes to write them as (0, 0)".to_owned())
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
