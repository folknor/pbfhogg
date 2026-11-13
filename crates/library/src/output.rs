//! The driver's fallible output boundary. Closed consumers are normal.
use std::io::{self, Write};
pub(crate) fn write(
    f: impl FnOnce(&mut io::BufWriter<io::StdoutLock<'_>>) -> io::Result<()>,
) -> io::Result<()> {
    let stream = io::stdout();
    let mut out = io::BufWriter::new(stream.lock());
    match f(&mut out).and_then(|()| out.flush()) {
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        result => result,
    }
}
pub(crate) fn error(message: &str) {
    let _ignored = writeln!(io::stderr().lock(), "pbfhogg-library: {message}");
}

/// Small protocol diagnostic count.
pub(crate) fn count(n: usize, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}
