//! Per-run scratch identity.
//!
//! Scratch files beside an input or output are named with the process id so
//! that concurrent processes cannot collide. Two runs inside one process share
//! that id, so every command that places scratch takes a [`RunToken`] at its
//! entry and appends [`RunToken::suffix`] after the pid. The first run of a
//! process gets token 0, whose suffix is empty, so a process that runs one
//! command - every CLI invocation - keeps the legacy names byte for byte.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Create a scratch fragment, retaining its path in any creation error.
pub(crate) fn create_file(path: &std::path::Path) -> crate::Result<std::fs::File> {
    std::fs::File::create(path).map_err(|e| {
        crate::Error::from(e).context(format!("failed to create scratch file {}", path.display()))
    })
}

/// One command run's scratch identity within this process.
#[derive(Debug)]
pub(crate) struct RunToken(u64);

static NEXT: AtomicU64 = AtomicU64::new(0);

impl RunToken {
    /// Take the next token. Tokens are never reused within a process; the
    /// space running out is an error rather than a wrap back to a token an
    /// earlier run may still hold.
    pub(crate) fn next() -> crate::Result<Self> {
        Self::next_from(&NEXT)
    }

    fn next_from(counter: &AtomicU64) -> crate::Result<Self> {
        let mut n = counter.load(Relaxed);
        loop {
            let next = n
                .checked_add(1)
                .ok_or_else(|| crate::Error::internal("scratch run token space exhausted"))?;
            match counter.compare_exchange_weak(n, next, Relaxed, Relaxed) {
                Ok(_) => return Ok(Self(n)),
                Err(actual) => n = actual,
            }
        }
    }

    /// The scratch-name suffix: empty for token 0, `-<n>` otherwise.
    pub(crate) fn suffix(&self) -> String {
        if self.0 == 0 {
            String::new()
        } else {
            format!("-{}", self.0)
        }
    }

    #[cfg(test)]
    fn from_raw(n: u64) -> Self {
        Self(n)
    }
}

#[cfg(test)]
mod tests {
    use super::RunToken;

    #[test]
    fn the_first_token_keeps_legacy_names() {
        assert_eq!(RunToken::from_raw(0).suffix(), "");
        assert_eq!(RunToken::from_raw(1).suffix(), "-1");
        assert_eq!(RunToken::from_raw(42).suffix(), "-42");
    }

    #[test]
    fn tokens_are_distinct() {
        let a = RunToken::next().expect("token");
        let b = RunToken::next().expect("token");
        assert_ne!(a.suffix(), b.suffix());
    }

    #[test]
    fn exhaustion_never_wraps_or_reuses_tokens() {
        let counter = std::sync::atomic::AtomicU64::new(u64::MAX - 1);
        assert_eq!(
            RunToken::next_from(&counter).expect("last token").0,
            u64::MAX - 1
        );
        for _ in 0..2 {
            let err = RunToken::next_from(&counter).expect_err("exhausted");
            assert_eq!(err.to_string(), "scratch run token space exhausted");
            assert_eq!(counter.load(super::Relaxed), u64::MAX);
        }
    }
}
