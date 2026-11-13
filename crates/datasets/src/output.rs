//! The tool's only output: operator narration on stderr. A closed stderr is
//! not an error worth failing a download for.
use std::io::Write;

pub(crate) fn note(message: &str) {
    let _ignored = writeln!(std::io::stderr().lock(), "{message}");
}

pub(crate) fn error(message: &str) {
    let _ignored = writeln!(std::io::stderr().lock(), "pbfhogg-datasets: {message}");
}
