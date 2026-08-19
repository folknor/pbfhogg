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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy)]
enum Kind {
    File,
    Dir,
}

/// Guards a path the *run itself created*.
///
/// Never point this at a path that may already hold user data: Drop deletes
/// it unconditionally. For a user-named `--output`, use [`OutputFile`],
/// which writes to a scratch sibling and renames into place, so a mid-run
/// failure cannot touch a pre-existing file of that name.
#[doc(hidden)]
pub struct PathGuard {
    path: PathBuf,
    kind: Kind,
    armed: bool,
}

impl PathGuard {
    /// Guard a file path. On Drop without `commit()`, `remove_file` is
    /// attempted best-effort.
    pub fn file(path: PathBuf) -> Self {
        Self {
            path,
            kind: Kind::File,
            armed: true,
        }
    }

    /// Guard a directory path. On Drop without `commit()`,
    /// `remove_dir_all` is attempted best-effort.
    pub fn dir(path: PathBuf) -> Self {
        Self {
            path,
            kind: Kind::Dir,
            armed: true,
        }
    }

    /// Release the guard so the path survives Drop. Returns the path
    /// for convenience at the commit site (e.g. renaming into place).
    pub fn commit(mut self) -> PathBuf {
        self.armed = false;
        std::mem::take(&mut self.path)
    }
}

impl Drop for PathGuard {
    fn drop(&mut self) {
        if self.armed {
            // Best-effort. A failure here is less important than the
            // error the caller is about to return; swallowing avoids
            // masking that original error with a cleanup error.
            match self.kind {
                Kind::File => drop(fs::remove_file(&self.path)),
                Kind::Dir => drop(fs::remove_dir_all(&self.path)),
            }
        }
    }
}

/// A user-named output file, written through a scratch sibling.
///
/// The destructive shape this replaces: create the output, guard it, fail
/// mid-run, and the guard deletes whatever was already at that path - so
/// `export -o important.json` against a bad input destroyed
/// `important.json`. Even without the guard, opening the final path for
/// write truncates it before the first byte of new output exists.
///
/// So the final path is not opened at all until the run has succeeded.
/// Callers write to [`OutputFile::path`] - a sibling in the same directory,
/// so [`OutputFile::commit`]'s rename never crosses a filesystem - and Drop
/// removes only that scratch file. Any pre-existing file at the destination
/// survives every failure by construction, and is replaced in a single
/// `rename` on success.
///
/// The replacement is atomic on Unix, which is the platform this crate is
/// built and tested for. `std::fs::rename` does replace an existing
/// destination on Windows too - it maps to `MoveFileEx` with
/// `MOVEFILE_REPLACE_EXISTING` - so the survives-on-failure guarantee holds
/// there as well, but the standard library does not promise atomicity for
/// that call, so this type does not either.
#[doc(hidden)]
pub struct OutputFile {
    temp: PathBuf,
    final_path: PathBuf,
    armed: bool,
}

impl OutputFile {
    /// Derive the scratch sibling for `final_path`. Does not create
    /// anything; the caller creates the file at [`OutputFile::path`].
    ///
    /// # Errors
    /// Returns an error if `final_path` has no file name.
    pub fn new(final_path: &Path) -> crate::error::Result<Self> {
        let name = final_path.file_name().ok_or_else(|| {
            crate::error::Error::from(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("output path has no file name: {}", final_path.display()),
            ))
        })?;
        // Counter as well as pid: one process can write several outputs
        // (multi-extract), and two of them in the same directory must not
        // collide on the scratch name.
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        // The marker is a *prefix*, not a suffix, so the scratch name still
        // ends with the name the user chose. Several writers decide their
        // encoding from the tail of the output path - `merge-changes` gzips
        // when it ends in `.gz` - and a suffixed scratch name would silently
        // flip that decision for the bytes actually written.
        let mut temp_name =
            std::ffi::OsString::from(format!(".pbfhogg-tmp-{}-{seq}.", std::process::id()));
        temp_name.push(name);
        let temp = final_path.with_file_name(temp_name);
        Ok(Self {
            temp,
            final_path: final_path.to_path_buf(),
            armed: true,
        })
    }

    /// The path to write to. Not the destination the user named.
    pub fn path(&self) -> &Path {
        &self.temp
    }

    /// Rename the scratch file over the destination and return it. Until
    /// this call the destination is untouched. An existing destination is
    /// replaced - atomically on Unix; see the type-level note for Windows.
    ///
    /// # Errors
    /// Returns an error if the rename fails.
    pub fn commit(mut self) -> crate::error::Result<PathBuf> {
        fs::rename(&self.temp, &self.final_path)?;
        self.armed = false;
        Ok(std::mem::take(&mut self.final_path))
    }
}

impl Drop for OutputFile {
    fn drop(&mut self) {
        if self.armed {
            // Best-effort, and only ever the scratch file.
            drop(fs::remove_file(&self.temp));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> tempfile::TempDir {
        // Never `std::env::temp_dir()`: process-id-only names collide
        // across the concurrent check sweeps, and the project keeps its
        // scratch inside the repo tree.
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
        fs::create_dir_all(&root).expect("create target dir");
        tempfile::Builder::new()
            .prefix("pbfhogg-path-guard-")
            .tempdir_in(root)
            .expect("create scratch dir")
    }

    #[test]
    fn file_drop_removes() {
        let dir = scratch();
        let tmp = dir.path().join("guarded.tmp");
        fs::write(&tmp, b"hello").expect("write test file");
        {
            let _g = PathGuard::file(tmp.clone());
        }
        assert!(!tmp.exists(), "file should be removed on drop");
    }

    #[test]
    fn file_commit_preserves() {
        let dir = scratch();
        let tmp = dir.path().join("committed.tmp");
        fs::write(&tmp, b"hello").expect("write test file");
        {
            let g = PathGuard::file(tmp.clone());
            let returned = g.commit();
            assert_eq!(returned, tmp);
        }
        assert!(tmp.exists(), "file should survive after commit");
    }

    #[test]
    fn dir_drop_removes_recursively() {
        let dir = scratch();
        let tmp = dir.path().join("guarded-dir");
        fs::create_dir_all(&tmp).expect("create test dir");
        fs::write(tmp.join("inside.txt"), b"x").expect("write test file");
        {
            let _g = PathGuard::dir(tmp.clone());
        }
        assert!(!tmp.exists(), "dir should be removed recursively on drop");
    }

    /// The data-loss regression: a failed run must not destroy - or even
    /// truncate - a pre-existing file that happens to carry the output
    /// name. `OutputFile` never opens the destination until commit.
    #[test]
    fn failed_run_leaves_preexisting_destination_intact() {
        let dir = scratch();
        let dest = dir.path().join("important.json");
        fs::write(&dest, b"user data").expect("write pre-existing file");
        {
            let out = OutputFile::new(&dest).expect("derive scratch path");
            fs::write(out.path(), b"partial output").expect("write partial output");
            // Guard dropped without commit: the failure path.
        }
        assert_eq!(
            fs::read(&dest).expect("destination still readable"),
            b"user data",
            "pre-existing destination must be byte-identical after a failed run"
        );
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .expect("read scratch dir")
            .filter_map(|e| e.ok().map(|e| e.file_name()))
            .filter(|n| n != "important.json")
            .collect();
        assert!(leftovers.is_empty(), "scratch file leaked: {leftovers:?}");
    }

    /// Writers that decide their encoding from the tail of the output path
    /// (`merge-changes` gzips when it ends in `.gz`) must see the same tail
    /// on the scratch file, so the uniquifying marker goes in front of the
    /// name, never behind it.
    #[test]
    fn scratch_path_keeps_the_destination_suffix() {
        let dir = scratch();
        let dest = dir.path().join("changes.osc.gz");
        let out = OutputFile::new(&dest).expect("derive scratch path");
        let name = out
            .path()
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .expect("scratch has a utf8 file name");
        assert!(
            name.ends_with(".osc.gz"),
            "scratch name {name} lost the destination suffix"
        );
        assert!(name.starts_with('.'), "scratch name {name} is not hidden");
        assert_eq!(
            out.path().parent(),
            dest.parent(),
            "scratch must be a sibling so commit's rename stays intra-filesystem"
        );
    }

    #[test]
    fn commit_replaces_destination_atomically() {
        let dir = scratch();
        let dest = dir.path().join("out.json");
        fs::write(&dest, b"old").expect("write pre-existing file");
        let out = OutputFile::new(&dest).expect("derive scratch path");
        fs::write(out.path(), b"new").expect("write output");
        let committed = out.commit().expect("commit");
        assert_eq!(committed, dest);
        assert_eq!(fs::read(&dest).expect("read destination"), b"new");
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .expect("read scratch dir")
            .filter_map(|e| e.ok().map(|e| e.file_name()))
            .filter(|n| n != "out.json")
            .collect();
        assert!(leftovers.is_empty(), "scratch file leaked: {leftovers:?}");
    }
}
