//! The verifier's only writers. Result lines go to stdout; check detail is
//! buffered and replayed to stdout only when the check does not pass (or
//! streamed live under `--verbose`); narration goes to stderr. A closed
//! stream is not worth failing a verification for.
use std::io::Write;

/// Check detail: held until the check's result is known, unless verbose.
#[derive(Debug, Default)]
pub(crate) struct Detail {
    verbose: bool,
    held: Vec<String>,
}

impl Detail {
    pub(crate) fn new(verbose: bool) -> Self {
        Self {
            verbose,
            held: Vec::new(),
        }
    }

    pub(crate) fn line(&mut self, line: &str) {
        if self.verbose {
            result(line);
        } else {
            self.held.push(line.to_owned());
        }
    }

    /// Forget the held detail: the check passed.
    pub(crate) fn discard(&mut self) {
        self.held.clear();
    }

    /// Write the held detail: the check did not pass.
    pub(crate) fn flush(&mut self) {
        let mut out = std::io::stdout().lock();
        for line in self.held.drain(..) {
            let _ignored = writeln!(out, "{line}");
        }
    }
}

/// One result line on stdout.
pub(crate) fn result(line: &str) {
    let _ignored = writeln!(std::io::stdout().lock(), "{line}");
}

/// Narration on stderr: run directory, executable identity, OSC scope.
pub(crate) fn note(message: &str) {
    let _ignored = writeln!(std::io::stderr().lock(), "{message}");
}

pub(crate) fn error(message: &str) {
    let _ignored = writeln!(std::io::stderr().lock(), "pbfhogg-verify: {message}");
}

/// A human duration: `850ms`, `15.1s`, `2m0s`.
pub(crate) fn duration(d: std::time::Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{}.{}s", ms / 1000, (ms % 1000) / 100)
    } else {
        format!("{}m{}s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    #[test]
    fn durations_read_at_a_glance() {
        // fixture-time: rendered values, not bounds.
        assert_eq!(super::duration(Duration::from_millis(850)), "850ms");
        // fixture-time: rendered values, not bounds.
        assert_eq!(super::duration(Duration::from_millis(15_140)), "15.1s");
        // fixture-time: rendered values, not bounds.
        assert_eq!(super::duration(Duration::from_secs(120)), "2m0s");
    }
}
