//! One verifier run: its private directory, its private copy of the
//! executable under test, and the inputs whose identity it watches.
//!
//! Every path a run writes is under `SCRATCH/tools/verify/RUNID/`, and a
//! RUNID is never reused (time, process ID and random bits, created with
//! `create_dir`, which refuses an existing name). A process that survived an
//! earlier run can therefore only write into that run's directory.
//!
//! Inputs: the brokkr machine lock, which `brokkr run` holds for the
//! verifier's whole life, keeps every cooperating writer (the dataset tool,
//! another verifier, a bench) away. Beyond that, each input's identity stamp
//! is taken when it is resolved and compared after every check: a moved
//! stamp turns the check into an environment outcome. That detects ordinary
//! external replacement; concurrent namespace manipulation by a process that
//! does not hold the lock is unsupported.
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::tunables;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

fn stamp(path: &Path) -> Result<Stamp, Error> {
    let meta = std::fs::metadata(path)
        .map_err(|e| Error::Environment(format!("{}: {e}", path.display())))?;
    Ok(Stamp {
        dev: meta.dev(),
        ino: meta.ino(),
        size: meta.size(),
        mtime: (meta.mtime(), meta.mtime_nsec()),
        ctime: (meta.ctime(), meta.ctime_nsec()),
    })
}

/// Detach a run-local hard link before taking its first input stamp.
fn private_input_copy(path: &Path) -> Result<(), Error> {
    let before = stamp(path)?;
    let tmp = path.with_file_name(format!(
        ".{}.watch-copy",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    let mut dest = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    let copied = (|| -> Result<(), Error> {
        let mut source = std::fs::File::open(path)?;
        std::io::copy(&mut source, &mut dest)?;
        dest.set_permissions(source.metadata()?.permissions())?;
        if stamp(path)? != before {
            return Err(Error::Environment(format!(
                "input {} changed while isolating its reference link",
                path.display()
            )));
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if copied.is_err() {
        drop(std::fs::remove_file(&tmp));
    }
    copied
}

pub(crate) struct Run {
    pub(crate) dir: PathBuf,
    /// The private copy every pbfhogg launch runs.
    pub(crate) pbfhogg: PathBuf,
    pub(crate) pbfhogg_digest: String,
    inputs: Vec<(PathBuf, Stamp)>,
    keep: bool,
}

impl Run {
    /// Create the run directory under `scratch` and snapshot `executable`
    /// into it.
    pub(crate) fn create(scratch: &Path, executable: &Path, keep: bool) -> Result<Self, Error> {
        let parent = scratch.join("tools").join("verify");
        std::fs::create_dir_all(&parent)
            .map_err(|e| Error::tool(format!("{}: {e}", parent.display())))?;
        let dir = parent.join(run_id()?);
        std::fs::create_dir(&dir).map_err(|e| Error::tool(format!("{}: {e}", dir.display())))?;
        let pbfhogg = dir.join("pbfhogg");
        std::fs::copy(executable, &pbfhogg)
            .map_err(|e| Error::tool(format!("copy {} into the run: {e}", executable.display())))?;
        std::fs::set_permissions(&pbfhogg, std::fs::Permissions::from_mode(0o500))?;
        let pbfhogg_digest = digest(&pbfhogg)?;
        Ok(Self {
            dir,
            pbfhogg,
            pbfhogg_digest,
            inputs: Vec::new(),
            keep,
        })
    }

    /// Watch `path` for the rest of the run.
    pub(crate) fn watch(&mut self, path: &Path) -> Result<(), Error> {
        if !self.inputs.iter().any(|(p, _)| p == path) {
            // Cache placement uses hard links. Once a reference artifact is
            // itself an input, isolate its inode: a later hit or eviction
            // changes the shared inode's ctime without changing these bytes.
            // Keep ctime in the input identity rather than masking mutations.
            if path.starts_with(&self.dir) && std::fs::metadata(path)?.nlink() > 1 {
                private_input_copy(path)?;
            }
            self.inputs.push((path.to_path_buf(), stamp(path)?));
        }
        Ok(())
    }

    /// `Err(Environment)` when a watched input moved since it was resolved.
    pub(crate) fn revalidate(&self) -> Result<(), Error> {
        for (path, then) in &self.inputs {
            if stamp(path)? != *then {
                return Err(Error::Environment(format!(
                    "input {} changed during the run; its comparisons mean nothing",
                    path.display()
                )));
            }
        }
        Ok(())
    }

    /// A fresh, empty directory for one check, `RUN/checks/NAME`.
    pub(crate) fn check_dir(&self, name: &str) -> Result<PathBuf, Error> {
        if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
            return Err(Error::tool(format!(
                "check directory must be one plain component, got {name:?}"
            )));
        }
        let dir = self.dir.join("checks").join(name);
        if dir.exists() {
            return Err(Error::tool(format!(
                "{} already exists in this run",
                dir.display()
            )));
        }
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    /// End of run. A run in which everything passed is removed unless kept;
    /// any other run is kept and named, so its evidence survives.
    pub(crate) fn finish(&self, all_passed: bool) {
        if all_passed && !self.keep {
            if let Err(e) = std::fs::remove_dir_all(&self.dir) {
                crate::output::error(&format!(
                    "cannot remove the run directory {}: {e}",
                    self.dir.display()
                ));
            }
        } else {
            crate::output::note(&format!("run directory kept: {}", self.dir.display()));
        }
    }
}

fn run_id() -> Result<String, Error> {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| Error::tool(e.to_string()))?
        .as_secs();
    let mut random = [0_u8; 8];
    // SAFETY: the buffer is writable for its whole length.
    let n = unsafe { libc::getrandom(random.as_mut_ptr().cast(), random.len(), 0) };
    if usize::try_from(n).ok() != Some(random.len()) {
        return Err(Error::tool(format!(
            "getrandom: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(format!(
        "{secs}-{}-{:016x}",
        std::process::id(),
        u64::from_le_bytes(random)
    ))
}

/// XXH3-128 of a file, 32 lowercase hex digits.
pub(crate) fn digest(path: &Path) -> Result<String, Error> {
    let mut file =
        std::fs::File::open(path).map_err(|e| Error::tool(format!("{}: {e}", path.display())))?;
    let mut hash = xxhash_rust::xxh3::Xxh3::new();
    let mut buf = vec![0; tunables::DIGEST_BUFFER];
    // loop-ends: end of file.
    loop {
        let n = match file.read(&mut buf) {
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            other => other?,
        };
        if n == 0 {
            return Ok(format!("{:032x}", hash.digest128()));
        }
        hash.update(&buf[..n]);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn fake_exe(dir: &Path) -> PathBuf {
        let exe = dir.join("exe");
        std::fs::write(&exe, b"bytes").unwrap();
        exe
    }

    #[test]
    fn runs_never_share_a_directory_and_run_a_private_copy() {
        let scratch = pbfhogg_test_scratch::scratch();
        let exe = fake_exe(scratch.path());
        let a = Run::create(scratch.path(), &exe, false).unwrap();
        let b = Run::create(scratch.path(), &exe, false).unwrap();
        assert_ne!(a.dir, b.dir);
        assert!(a.pbfhogg.starts_with(&a.dir));
        assert_eq!(a.pbfhogg_digest, digest(&exe).unwrap());
        std::fs::write(&exe, b"rebuilt").unwrap();
        assert_eq!(std::fs::read(&a.pbfhogg).unwrap(), b"bytes");
    }

    #[test]
    fn check_dirs_are_fresh_and_plain() {
        let scratch = pbfhogg_test_scratch::scratch();
        let run = Run::create(scratch.path(), &fake_exe(scratch.path()), false).unwrap();
        let dir = run.check_dir("sort").unwrap();
        assert!(dir.is_dir());
        assert!(run.check_dir("sort").is_err());
        for bad in ["", ".", "..", "a/b"] {
            assert!(run.check_dir(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_replaced_input_is_an_environment_outcome() {
        let scratch = pbfhogg_test_scratch::scratch();
        let mut run = Run::create(scratch.path(), &fake_exe(scratch.path()), false).unwrap();
        let input = scratch.path().join("in.pbf");
        std::fs::write(&input, b"one").unwrap();
        run.watch(&input).unwrap();
        run.revalidate().unwrap();
        let staged = scratch.path().join("staged");
        std::fs::write(&staged, b"two").unwrap();
        std::fs::rename(&staged, &input).unwrap();
        assert!(matches!(run.revalidate(), Err(Error::Environment(_))));
    }

    #[test]
    fn watched_reference_links_survive_cache_relink_and_eviction() {
        let scratch = pbfhogg_test_scratch::scratch();
        let mut run = Run::create(scratch.path(), &fake_exe(scratch.path()), false).unwrap();
        let artifact = scratch.path().join("cached.pbf");
        std::fs::write(&artifact, b"reference bytes").unwrap();
        let input = run.check_dir("first").unwrap().join("reference.pbf");
        std::fs::hard_link(&artifact, &input).unwrap();
        run.watch(&input).unwrap();
        assert_ne!(
            std::fs::metadata(&input).unwrap().ino(),
            std::fs::metadata(&artifact).unwrap().ino()
        );
        let hit = run.check_dir("second").unwrap().join("reference.pbf");
        std::fs::hard_link(&artifact, &hit).unwrap();
        std::fs::remove_file(&artifact).unwrap();
        run.revalidate().unwrap();
        // A real change of the private input still fails identity checks.
        std::fs::write(&input, b"changed").unwrap();
        assert!(matches!(run.revalidate(), Err(Error::Environment(_))));
    }

    #[test]
    fn a_passing_run_is_removed_and_a_kept_one_stays() {
        let scratch = pbfhogg_test_scratch::scratch();
        let exe = fake_exe(scratch.path());
        let run = Run::create(scratch.path(), &exe, false).unwrap();
        run.finish(true);
        assert!(!run.dir.exists());
        let kept = Run::create(scratch.path(), &exe, true).unwrap();
        kept.finish(true);
        assert!(kept.dir.exists());
        let failed = Run::create(scratch.path(), &exe, false).unwrap();
        failed.finish(false);
        assert!(failed.dir.exists());
    }
}
