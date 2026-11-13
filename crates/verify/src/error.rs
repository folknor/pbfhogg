//! How a verifier step ends when it does not complete.

/// A step that did not complete. Only `Failed` is a statement about pbfhogg;
/// the others say the comparison could not be made.
#[derive(Debug)]
pub(crate) enum Error {
    /// SIGTERM, SIGINT or SIGHUP reached the verifier: neither PASS nor FAIL.
    Interrupted,
    /// Outside the code under test: a reference tool killed by SIGKILL, an
    /// input that moved during the check, a descendant that outlived its
    /// launch.
    Environment(String),
    /// A comparison found pbfhogg wrong, or pbfhogg failed a run it had to
    /// complete.
    Failed(String),
    /// The verifier could not establish that every descendant of a launch is
    /// gone. No later launch, comparison or cache publication is safe, so a
    /// suite stops here and keeps its run directory.
    Unsettled(String),
    /// The verifier could not do its own work: configuration, I/O, an
    /// unsupported comparison domain. Malformed tool reports are `Failed`.
    Tool(String),
}

impl Error {
    pub(crate) fn tool(message: impl Into<String>) -> Self {
        Self::Tool(message.into())
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Interrupted => f.write_str("interrupted"),
            Self::Environment(why) => write!(f, "environment: {why}"),
            Self::Unsettled(why) => write!(f, "quiescence unestablished: {why}"),
            Self::Failed(why) | Self::Tool(why) => f.write_str(why),
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Tool(e.to_string())
    }
}

impl From<pbfhogg_devkit::Error> for Error {
    fn from(e: pbfhogg_devkit::Error) -> Self {
        Self::Tool(e.to_string())
    }
}
