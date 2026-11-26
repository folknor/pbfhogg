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

/// An input's identity: device, inode, size and mtime. ctime is left out on
/// purpose. Reference artifacts reach checks as hard links of the cache's
/// inode, and every later placement or eviction of that inode moves its
/// ctime without touching its bytes; keeping ctime here forced a full private
/// copy of each such artifact before it could be watched. The verifier is a
/// local single-user tool, so a same-size rewrite that leaves mtime equal is
/// outside what it defends against, the same stamp policy cache lookups use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stamp {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: (i64, i64),
}

pub(crate) fn stamp(path: &Path) -> Result<Stamp, Error> {
    let meta = std::fs::metadata(path)
        .map_err(|e| Error::Environment(format!("{}: {e}", path.display())))?;
    Ok(Stamp::of(&meta))
}

impl Stamp {
    fn of(meta: &std::fs::Metadata) -> Self {
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.size(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
        }
    }
}

pub(crate) struct Run {
    pub(crate) dir: PathBuf,
    /// The private copy every pbfhogg launch runs.
    pub(crate) pbfhogg: PathBuf,
    pub(crate) pbfhogg_digest: String,
    inputs: Vec<(PathBuf, Stamp)>,
    /// Content digests by path, each valid only while the path's stamp
    /// still equals the one it was hashed under. Reference cache keys name
    /// the same dataset input, tool and libraries on every call; without
    /// this each call reread them in full.
    digests: std::collections::HashMap<PathBuf, (Stamp, String)>,
    /// The run's retained OPL decodings, in `RUN/decodings/`: owned by the
    /// run, so no other run ever reads them.
    pub(crate) decodings: crate::opl_compare::Decodings,
    /// Expected OPL projections shared by the re-encoding checks.
    pub(crate) reencode_references: crate::verify_reencode::ReferenceStore,
    pub(crate) frames: crate::verify_reencode::FrameStore,
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
            digests: std::collections::HashMap::new(),
            decodings: crate::opl_compare::Decodings::default(),
            reencode_references: crate::verify_reencode::ReferenceStore::default(),
            frames: crate::verify_reencode::FrameStore::default(),
            keep,
        })
    }

    /// Watch `path` for the rest of the run.
    pub(crate) fn watch(&mut self, path: &Path) -> Result<(), Error> {
        if !self.inputs.iter().any(|(p, _)| p == path) {
            // A cache-placed reference artifact is watched on the cache's
            // own inode, uncopied: the stamp leaves out the ctime that later
            // placements and evictions move (see `Stamp`).
            self.inputs.push((path.to_path_buf(), stamp(path)?));
        }
        Ok(())
    }

    /// XXH3-128 of `path`, reused while the path's stamp (device, inode,
    /// size, mtime) is the one it was hashed under. A first hash reads
    /// through one handle whose stamp must equal the path's before and after
    /// the read, so the memo never holds one file's digest under another's
    /// stamp; a file seen changing meanwhile is ENVIRONMENT. This is an
    /// observation under the input identity contract, not a snapshot: it
    /// neither replaces nor refreshes the watched stamps `revalidate` checks.
    pub(crate) fn digest_of(&mut self, path: &Path) -> Result<String, Error> {
        let now = stamp(path)?;
        if let Some((then, hex)) = self.digests.get(path)
            && *then == now
        {
            return Ok(hex.clone());
        }
        let mut file = std::fs::File::open(path)
            .map_err(|e| Error::tool(format!("{}: {e}", path.display())))?;
        let changed = || {
            Error::Environment(format!(
                "{} changed while its digest was taken",
                path.display()
            ))
        };
        if Stamp::of(&file.metadata()?) != now {
            return Err(changed());
        }
        let hex = digest_reader(&mut file)?;
        if Stamp::of(&file.metadata()?) != now || stamp(path)? != now {
            return Err(changed());
        }
        self.digests.insert(path.to_path_buf(), (now, hex.clone()));
        Ok(hex)
    }

    /// `Err(Environment)` when a watched input moved since it was resolved.
    ///
    /// Every pass stats every watched path, so the cost grows with launches
    /// times watches; at tens of each per check it is a few hundred
    /// `stat`s, left deliberately. A watch outside the current launch's
    /// inputs can still bear on the verdict or on cache publication, and
    /// the calls before a launch, after it, at publication and at a check's
    /// end guard different transitions; any narrowing has to show what can
    /// change between the calls it collapses.
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
    digest_reader(&mut file)
}

fn digest_reader(file: &mut std::fs::File) -> Result<String, Error> {
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
        crate::test_support::refused(
            run.check_dir("sort"),
            "checks/sort already exists in this run",
        );
        for bad in ["", ".", "..", "a/b"] {
            crate::test_support::refused(
                run.check_dir(bad),
                "check directory must be one plain component",
            );
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
    fn a_digest_is_reused_only_while_its_stamp_holds() {
        let scratch = pbfhogg_test_scratch::scratch();
        let mut run = Run::create(scratch.path(), &fake_exe(scratch.path()), false).unwrap();
        let input = scratch.path().join("in.pbf");
        std::fs::write(&input, b"one").unwrap();
        let first = run.digest_of(&input).unwrap();
        assert_eq!(first, digest(&input).unwrap());
        assert_eq!(run.digest_of(&input).unwrap(), first);
        // A replacement moves the stamp, so the content is hashed again.
        let staged = scratch.path().join("staged");
        std::fs::write(&staged, b"two").unwrap();
        std::fs::rename(&staged, &input).unwrap();
        let second = run.digest_of(&input).unwrap();
        assert_ne!(second, first);
        assert_eq!(second, digest(&input).unwrap());
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
        // Watched in place: no private copy is made.
        assert_eq!(
            std::fs::metadata(&input).unwrap().ino(),
            std::fs::metadata(&artifact).unwrap().ino()
        );
        let hit = run.check_dir("second").unwrap().join("reference.pbf");
        std::fs::hard_link(&artifact, &hit).unwrap();
        std::fs::remove_file(&artifact).unwrap();
        run.revalidate().unwrap();
        // A write through another link of the shared inode still fails
        // identity checks.
        std::fs::write(&hit, b"changed").unwrap();
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
