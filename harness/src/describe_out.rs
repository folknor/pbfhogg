//! The one place the harness touches stdout: the describe document.

/// Write `text` to stdout and flush it. `false` if any of it failed (a
/// reader gone, say); the caller then exits 2 without a word.
pub(crate) fn write_document(text: &str) -> bool {
    use std::io::Write as _;
    let mut out = std::io::stdout().lock();
    out.write_all(text.as_bytes()).is_ok() && out.write_all(b"\n").is_ok() && out.flush().is_ok()
}
