//! RAII guard for output / scratch paths.
//!
//! A [`PathGuard`] wraps a `PathBuf` and removes the pointed-at file or
//! directory when dropped, unless [`PathGuard::commit`] was called
//! first. Use it to avoid leaving partial output or scratch state on
//! disk when a command errors mid-stream.
//!
//! Happy-path cost is a single `Option::take`; the actual filesystem
//! work only runs on the error path.
//!
//! See `decisions/0003-error-path-hygiene-via-pathguard.md` for the
//! policy context: when to use this primitive, the "counters bump
//! after successful write" rule that accompanies it, and the
//! checklist for adding a new command.

use std::fs;
use std::path::PathBuf;

#[derive(Clone, Copy)]
enum Kind {
    File,
    Dir,
}

#[doc(hidden)]
pub struct PathGuard {
    path: Option<PathBuf>,
    kind: Kind,
}

impl PathGuard {
    /// Guard a file path. On Drop without `commit()`, `remove_file` is
    /// attempted best-effort.
    pub fn file(path: PathBuf) -> Self {
        Self {
            path: Some(path),
            kind: Kind::File,
        }
    }

    /// Guard a directory path. On Drop without `commit()`,
    /// `remove_dir_all` is attempted best-effort.
    pub fn dir(path: PathBuf) -> Self {
        Self {
            path: Some(path),
            kind: Kind::Dir,
        }
    }

    /// Release the guard so the path survives Drop. Returns the path
    /// for convenience at the commit site (e.g. renaming into place).
    pub fn commit(mut self) -> PathBuf {
        self.path
            .take()
            .expect("PathGuard::commit called after path released")
    }
}

impl Drop for PathGuard {
    fn drop(&mut self) {
        if let Some(p) = self.path.take() {
            // Best-effort. A failure here is less important than the
            // error the caller is about to return; swallowing avoids
            // masking that original error with a cleanup error.
            match self.kind {
                Kind::File => drop(fs::remove_file(&p)),
                Kind::Dir => drop(fs::remove_dir_all(&p)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_drop_removes() {
        let tmp = std::env::temp_dir().join(format!(
            "pbfhogg-path-guard-test-{}.tmp",
            std::process::id()
        ));
        fs::write(&tmp, b"hello").expect("write test file");
        {
            let _g = PathGuard::file(tmp.clone());
        }
        assert!(!tmp.exists(), "file should be removed on drop");
    }

    #[test]
    fn file_commit_preserves() {
        let tmp = std::env::temp_dir().join(format!(
            "pbfhogg-path-guard-commit-{}.tmp",
            std::process::id()
        ));
        fs::write(&tmp, b"hello").expect("write test file");
        {
            let g = PathGuard::file(tmp.clone());
            let returned = g.commit();
            assert_eq!(returned, tmp);
        }
        assert!(tmp.exists(), "file should survive after commit");
        fs::remove_file(&tmp).expect("cleanup test file");
    }

    #[test]
    fn dir_drop_removes_recursively() {
        let tmp =
            std::env::temp_dir().join(format!("pbfhogg-path-guard-dir-{}", std::process::id()));
        fs::create_dir_all(&tmp).expect("create test dir");
        fs::write(tmp.join("inside.txt"), b"x").expect("write test file");
        {
            let _g = PathGuard::dir(tmp.clone());
        }
        assert!(!tmp.exists(), "dir should be removed recursively on drop");
    }
}
