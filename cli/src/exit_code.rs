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

/// Advice for an output refusal, naming the flag that grants what was
/// refused. The library error names no flag; this front end does. Walks the
/// chain the way [`is_platform_unsupported`] does, so a refusal raised at
/// publication inside a command is found too.
pub fn output_hint(err: &(dyn std::error::Error + 'static)) -> Option<&'static str> {
    let mut current = Some(err);
    while let Some(error) = current {
        current = if let Some(error) = error.downcast_ref::<pbfhogg::Error>() {
            match error.kind() {
                pbfhogg::ErrorKind::Output(pbfhogg::OutputError::Exists { .. }) => {
                    return Some("; pass --overwrite (-O) to replace it");
                }
                pbfhogg::ErrorKind::Output(pbfhogg::OutputError::ExclusivePublishUnsupported {
                    ..
                }) => {
                    return Some("; pass --overwrite (-O) to publish without the exclusive check");
                }
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
