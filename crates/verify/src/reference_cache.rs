//! Completed reference executions, plus summaries of osmium's OPL decodings
//! (`Summary`); no verdict is cached.
use crate::error::Error;
use crate::output;
use crate::reference::{RunArg, ToolRun};
use crate::run::digest;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
const FORMAT: u32 = 2;
const MANIFEST: &str = "manifest.json";
const ARTIFACTS: &str = "artifacts";
const STDOUT: &str = "stdout";
const STDERR: &str = "stderr";
const STAGING_PREFIX: &str = ".staging-";
const TRASH_PREFIX: &str = ".trash-";
#[derive(serde::Serialize, serde::Deserialize)]
struct Manifest {
    format: u32,
    key: String,
    exit_code: i32,
    artifacts: Vec<Artifact>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, PartialEq, Eq)]
struct Artifact {
    name: String,
    xxh128: String,
    stamp: Stamp,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
struct Stamp {
    dev: u64,
    ino: u64,
    size: u64,
    mtime_ns: i128,
}

impl Stamp {
    fn of(meta: &fs::Metadata) -> Self {
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.size(),
            mtime_ns: i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
        }
    }
}

pub(crate) struct ReferenceCache {
    pub(crate) dir: PathBuf,
    pub(crate) run_id: String,
    pub(crate) fresh: bool,
    /// The tool's `--version` and `ldd` answers, probed once per run and
    /// reused while what they were observed against still holds.
    probed: Option<Probed>,
    /// Staging directories made so far this run. Each staging name carries
    /// its own number beside the run ID and the `ToolRun` ID, so no launch
    /// meets an earlier one's leftover tree (a run ID is reused by every
    /// call, and a staging tree an unsettled or failed launch left behind
    /// stays until the next run's startup sweep).
    stagings: u64,
}

/// One run's probe of the reference tool, bound to what the probe saw: the
/// executable's digest, and for each library path `ldd` printed, its
/// canonical target and that target's digest. Any of those moving means the
/// answers may no longer describe the tool a launch would load, so the memo
/// is dropped and the probes run again. A new probe resolves the closure
/// twice and keeps its answers only when the binding held across both.
struct Probed {
    program: PathBuf,
    program_digest: String,
    version: String,
    libs: Vec<(PathBuf, PathBuf, String)>,
}

impl Probed {
    /// Whether every observation the probe answers were bound to still holds.
    fn holds(&self, watched: &mut crate::run::Run, program: &Path) -> Result<bool, Error> {
        if self.program != program || watched.digest_of(program)? != self.program_digest {
            return Ok(false);
        }
        for (raw, canonical, hex) in &self.libs {
            if fs::canonicalize(raw).ok().as_ref() != Some(canonical)
                || watched.digest_of(canonical)? != *hex
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn text(&self) -> String {
        let mut libs: Vec<(&PathBuf, &String)> =
            self.libs.iter().map(|(_, c, hex)| (c, hex)).collect();
        libs.sort();
        libs.dedup();
        let mut text = format!(
            "tool {} {}\nversion {:?}\n",
            self.program.display(),
            self.program_digest,
            self.version
        );
        for (path, hex) in libs {
            text.push_str(&format!("lib {} {hex}\n", path.display()));
        }
        text
    }
}
impl ReferenceCache {
    pub(crate) fn new(scratch: &Path, run_id: String, fresh: bool) -> Result<Self, Error> {
        let dir = scratch.join("tools/verify-reference");
        fs::create_dir_all(&dir)?;
        // Old writers can only reach their unique old paths. Never sweep
        // this run's staging, and never follow a symlink during removal.
        for entry in fs::read_dir(&dir)?.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if (name.starts_with(STAGING_PREFIX)
                && !name.starts_with(&format!("{STAGING_PREFIX}{run_id}-")))
                || name.starts_with(TRASH_PREFIX)
            {
                remove_tree(&entry.path());
            }
        }
        Ok(Self {
            dir,
            run_id,
            fresh,
            probed: None,
            stagings: 0,
        })
    }

    /// A fresh staging tree for `run`, with its artifact, home and scratch
    /// directories. The root is pushed onto `track` as soon as it exists, so
    /// a failure creating a subdirectory still leaves it owned.
    fn staging(&mut self, run: &ToolRun, track: &mut Vec<PathBuf>) -> Result<PathBuf, Error> {
        self.stagings += 1;
        let staging = self.dir.join(format!(
            "{STAGING_PREFIX}{}-{}-{}",
            self.run_id, run.id, self.stagings
        ));
        fs::create_dir(&staging)?;
        track.push(staging.clone());
        for sub in [ARTIFACTS, "home", "scratch"] {
            fs::create_dir(staging.join(sub))?;
        }
        Ok(staging)
    }
    /// Keep the cache within `budget` logical bytes (the artifact sizes its
    /// manifests record): the most recently used entries that fit stay,
    /// recency being a separate `used` file (or the original manifest); an
    /// entry larger than the budget goes too, and zero keeps none. `None`
    /// keeps everything. Called once a verify's reference requests are done,
    /// under its lock, so no entry is still to be placed or read; the run's
    /// outputs are their own links and stay. An entry whose manifest cannot
    /// be read is invalid, which lookup would remove, and goes. Each eviction
    /// renames the entry to its trash name first, so one cut short is swept
    /// with the other leftovers by the next verify; a failure is a warning,
    /// never the verify's result. The order is recency alone, not recompute
    /// cost per byte: a cost-only order would pin entries of a stale input
    /// for good, and a cost-aware order with aging (GreedyDual-Size) changes
    /// only which misses recur when a dataset's working set exceeds the
    /// budget, a payoff no recorded run has measured yet.
    pub(crate) fn retain(&self, budget: Option<u64>) {
        let Some(budget) = budget else {
            return;
        };
        // Summaries are a few hundred bytes each, so they are bounded by
        // count, not by the artifact budget; a zero budget keeps none.
        self.retain_summaries(if budget == 0 {
            0
        } else {
            crate::tunables::SUMMARY_RETAIN
        });
        let read = match fs::read_dir(&self.dir) {
            Ok(read) => read,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                output::note(&format!(
                    "reference cache {} not read, budget not applied: {e}",
                    self.dir.display()
                ));
                return;
            }
        };
        let mut unreadable = 0usize;
        let mut entries: Vec<(std::time::SystemTime, Option<u64>, PathBuf)> = read
            .filter_map(|entry| entry.map_err(|_| unreadable += 1).ok())
            .filter(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                name.len() == 32 && name.bytes().all(|b| b.is_ascii_hexdigit())
            })
            .map(|entry| {
                let manifest = entry.path().join(MANIFEST);
                let used = fs::metadata(entry.path().join("used"))
                    .or_else(|_| fs::metadata(&manifest))
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::UNIX_EPOCH);
                let bytes = (if fs::symlink_metadata(entry.path()).is_ok_and(|m| m.is_dir())
                    && fs::symlink_metadata(&manifest).is_ok_and(|m| m.is_file())
                {
                    fs::read_to_string(&manifest)
                } else {
                    Err(std::io::Error::other("invalid entry"))
                })
                .ok()
                .and_then(|text| serde_json::from_str::<Manifest>(&text).ok())
                .map(|m| {
                    m.artifacts
                        .iter()
                        .fold(0_u64, |sum, a| sum.saturating_add(a.stamp.size))
                });
                (used, bytes, entry.path())
            })
            .collect();
        if unreadable > 0 {
            output::note(&format!(
                "reference cache {}: {unreadable} entries could not be read and stay",
                self.dir.display()
            ));
        }
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.0));
        let mut kept = 0u64;
        let (mut evicted, mut freed) = (0usize, 0u64);
        for (_, bytes, path) in entries {
            // A zero budget keeps none, an entry of no logical bytes included.
            if budget > 0
                && let Some(bytes) = bytes
                && kept.saturating_add(bytes) <= budget
            {
                kept += bytes;
                continue;
            }
            let trash = path.with_file_name(format!(
                "{TRASH_PREFIX}{}",
                path.file_name().unwrap_or_default().to_string_lossy()
            ));
            match fs::rename(&path, &trash) {
                Ok(()) => {
                    remove_tree(&trash);
                    evicted += 1;
                    freed += bytes.unwrap_or(0);
                }
                Err(e) => output::note(&format!(
                    "reference cache entry {} not evicted: {e}",
                    path.display()
                )),
            }
        }
        if evicted > 0 {
            output::note(&format!(
                "reference cache: evicted {evicted} entries ({freed} logical bytes), {kept} bytes kept of a {budget} byte budget",
            ));
        }
    }

    /// The entry at `entry` when it is complete, valid for `key` and today's
    /// accepted outcomes; an invalid one is removed.
    fn lookup(
        &self,
        entry: &Path,
        key: &Key,
        run: &ToolRun,
        outputs: &[(String, &Path)],
    ) -> Option<Manifest> {
        if let Err(e) = fs::symlink_metadata(entry)
            && e.kind() == std::io::ErrorKind::NotFound
        {
            return None;
        }
        let verdict = (|| -> Result<Manifest, String> {
            if !fs::symlink_metadata(entry).is_ok_and(|m| m.is_dir())
                || !fs::symlink_metadata(entry.join(ARTIFACTS)).is_ok_and(|m| m.is_dir())
                || !fs::symlink_metadata(entry.join(MANIFEST)).is_ok_and(|m| m.is_file())
            {
                return Err("cache entry is not a regular directory tree".into());
            }
            let text = fs::read_to_string(entry.join(MANIFEST))
                .map_err(|e| format!("manifest unreadable: {e}"))?;
            let manifest: Manifest =
                serde_json::from_str(&text).map_err(|e| format!("manifest unreadable: {e}"))?;
            if manifest.format != FORMAT || manifest.key != key.text {
                return Err("manifest does not match the key".into());
            }
            if !run.accepted.contains(&manifest.exit_code) {
                return Err(format!(
                    "stored exit {} is not accepted",
                    manifest.exit_code
                ));
            }
            let mut expected: Vec<&str> = outputs.iter().map(|(n, _)| n.as_str()).collect();
            expected.extend([STDOUT, STDERR]);
            expected.sort_unstable();
            let mut held: Vec<&str> = manifest.artifacts.iter().map(|a| a.name.as_str()).collect();
            held.sort_unstable();
            if held != expected {
                return Err("artifact set differs from the run's".into());
            }
            for artifact in &manifest.artifacts {
                let path = entry.join(ARTIFACTS).join(&artifact.name);
                let meta = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
                if !meta.is_file() {
                    return Err(format!("{} is not a regular file", artifact.name));
                }
                // The recorded stamp (device, inode, size, mtime) stands in
                // for the bytes: published artifacts are read-only and
                // nothing in a local single-user run rewrites them in place
                // and sets mtime back. Placement links and evictions move
                // only nlink and ctime, which the stamp leaves out. A
                // moved stamp (a remount, say) falls back to the full digest.
                if Stamp::of(&meta) != artifact.stamp
                    && digest(&path).map_err(|e| e.to_string())? != artifact.xxh128
                {
                    return Err(format!("{} no longer matches its digest", artifact.name));
                }
            }
            // Read only once its stamp or digest held above.
            if run.conditional(manifest.exit_code) {
                let stderr = fs::read(entry.join(ARTIFACTS).join(STDERR))
                    .map_err(|e| format!("{STDERR}: {e}"))?;
                if !run.completes(manifest.exit_code, &stderr) {
                    return Err(format!(
                        "stored exit {} without the stderr that completes it",
                        manifest.exit_code
                    ));
                }
            }
            Ok(manifest)
        })();
        match verdict {
            Ok(manifest) => Some(manifest),
            Err(why) => {
                output::note(&format!("  reference cache entry dropped: {why}"));
                remove_tree(entry);
                None
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Key {
    pub(crate) text: String,
    hex: String,
}

/// The format of a stored decoding summary; bump it with any change to what
/// a summary means: the OPL framing hashed, the key parse, the strict rank
/// or the counting.
const SUMMARY_FORMAT: u32 = 1;
const SUMMARIES: &str = "summaries";

/// osmium's reading of one exact input under one OPL policy, observed by the
/// verifier while it read that decoding whole without a read or parse error:
/// whether the objects strictly ascend in object order, the object count per
/// kind, and the XXH3-128 of the whole OPL byte stream, newlines included.
/// It is an observation of bytes osmium decoded, never a verdict: every
/// comparison still decides from two summaries, or from the full merge.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
pub(crate) struct Summary {
    pub(crate) ordered: bool,
    pub(crate) counts: [u64; 3],
    pub(crate) digest: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct StoredSummary {
    format: u32,
    key: String,
    summary: Summary,
}
/// Seal `staging` and make it the entry at `entry`, replacing any older one.
fn publish(
    staging: &Path,
    entry: &Path,
    key: &str,
    exit_code: i32,
    outputs: &[(String, &Path)],
    checkpoint: impl FnOnce() -> Result<(), Error>,
) -> Result<(), Error> {
    let artifacts_dir = staging.join(ARTIFACTS);
    let mut names: Vec<&str> = outputs.iter().map(|(n, _)| n.as_str()).collect();
    names.extend([STDOUT, STDERR]);
    let mut artifacts = Vec::new();
    for name in names {
        let path = artifacts_dir.join(name);
        let meta = fs::symlink_metadata(&path)?;
        if !meta.is_file() {
            return Err(Error::tool(format!("{name} is not a regular file")));
        }
        // Kept: paid only on a miss, after a full tool run, and it makes a
        // published entry whole across a crash rather than a torn file the
        // next lookup would have to drop and run again.
        fs::File::open(&path)?.sync_all()?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444))?;
        let xxh128 = digest(&path)?;
        artifacts.push(Artifact {
            name: name.to_owned(),
            xxh128,
            stamp: Stamp::of(&fs::symlink_metadata(&path)?),
        });
    }
    fs::remove_file(staging.join("probe")).or_else(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(e)
        }
    })?;
    // Only the artifacts and the manifest are published.
    for scratch in ["home", "scratch"] {
        fs::remove_dir_all(staging.join(scratch))?;
    }
    write_manifest(
        &staging.join(MANIFEST),
        &Manifest {
            format: FORMAT,
            key: key.to_owned(),
            exit_code,
            artifacts,
        },
    )?;
    sync_dir(&artifacts_dir)?;
    sync_dir(staging)?;
    checkpoint()?;
    if entry.exists() {
        let trash = entry.with_file_name(format!(
            "{TRASH_PREFIX}{}",
            entry.file_name().unwrap_or_default().to_string_lossy()
        ));
        fs::rename(entry, &trash)?;
        fs::rename(staging, entry)?;
        remove_tree(&trash);
    } else {
        fs::rename(staging, entry)?;
    }
    if let Some(parent) = entry.parent() {
        sync_dir(parent)?;
    }
    Ok(())
}

fn write_manifest(path: &Path, manifest: &Manifest) -> Result<(), Error> {
    let tmp = path.with_extension("json.tmp");
    let mut file = fs::File::create(&tmp)?;
    file.write_all(
        serde_json::to_string_pretty(manifest)
            .map_err(|e| Error::tool(e.to_string()))?
            .as_bytes(),
    )?;
    file.sync_all()?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn sync_dir(dir: &Path) -> std::io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

/// Put each artifact of `source` at its check path, and stdout at `sink`.
/// Only a complete run is placed, so every artifact is required and must
/// still be a regular file: the post-run identity probe launches the tool
/// again in the staging tree, so a source the completion probe accepted can
/// have changed since. Destinations must be absent. On failure the names this
/// placement made are removed.
fn place(source: &Path, outputs: &[(String, &Path)], sink: &Path) -> Result<(), Error> {
    let mut targets: Vec<(&str, &Path)> = outputs.iter().map(|(n, p)| (n.as_str(), *p)).collect();
    targets.push((STDOUT, sink));
    let mut made: Vec<PathBuf> = Vec::new();
    let result = (|| -> Result<(), Error> {
        for (name, dest) in targets {
            let src = source.join(name);
            if !fs::symlink_metadata(&src).is_ok_and(|m| m.is_file()) {
                return Err(Error::tool(format!(
                    "reference artifact {name} is no longer a regular file at placement"
                )));
            }
            place_one(&src, dest)?;
            made.push(dest.to_path_buf());
        }
        Ok(())
    })();
    if result.is_err() {
        for path in made {
            drop(fs::remove_file(&path));
        }
    }
    result
}

fn place_one(src: &Path, dest: &Path) -> Result<(), Error> {
    if fs::symlink_metadata(dest).is_ok() {
        return Err(Error::tool(format!(
            "reference artifact destination {} already exists",
            dest.display()
        )));
    }
    match fs::hard_link(src, dest) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            let tmp = dest.with_file_name(format!(
                ".{}.reference-copy",
                dest.file_name().unwrap_or_default().to_string_lossy()
            ));
            let copied = (|| -> Result<(), Error> {
                fs::copy(src, &tmp)?;
                fs::File::open(&tmp)?.sync_all()?;
                if fs::symlink_metadata(dest).is_ok() {
                    return Err(Error::tool(format!(
                        "reference artifact destination {} appeared during the copy",
                        dest.display()
                    )));
                }
                fs::rename(&tmp, dest)?;
                Ok(())
            })();
            if copied.is_err() {
                drop(fs::remove_file(&tmp));
            }
            copied
        }
        Err(e) => Err(Error::tool(format!(
            "cannot place reference artifact {}: {e}",
            dest.display()
        ))),
    }
}

/// How a reference execution settled. Completion is decided once, here, from
/// the same probe that gates publication and placement, so the caller never
/// re-derives it from the exit status alone.
#[derive(Debug)]
pub(crate) enum Settled {
    /// An accepted outcome with every declared output and stdout present as
    /// regular files, placed at the check's paths. `cached` is a lookup hit.
    Complete {
        ended: crate::process::Ended,
        cached: bool,
    },
    /// The exit, or its stderr condition, is not accepted; a signal death is
    /// here too. Nothing is placed.
    Unaccepted { ended: crate::process::Ended },
    /// An accepted outcome that left declared artifacts missing or not
    /// regular files, each with its reason. Nothing is placed.
    InvalidOutputs {
        ended: crate::process::Ended,
        invalid: Vec<(String, &'static str)>,
    },
}

/// Each of `names` under `dir` that is not a regular file, with the reason.
/// A metadata error other than absence is the verifier's own I/O failure.
fn invalid_artifacts<'a>(
    dir: &Path,
    names: impl Iterator<Item = &'a str>,
) -> Result<Vec<(String, &'static str)>, Error> {
    let mut invalid = Vec::new();
    for name in names {
        match fs::symlink_metadata(dir.join(name)) {
            Ok(m) if m.is_file() => {}
            Ok(_) => invalid.push((name.to_owned(), "not a regular file")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                invalid.push((name.to_owned(), "missing"));
            }
            Err(e) => {
                return Err(Error::tool(format!(
                    "cannot inspect reference artifact {name}: {e}"
                )));
            }
        }
    }
    Ok(invalid)
}

/// Settle a fresh run from its exit and its staged `artifacts`. The exit is
/// judged before any artifact is probed, so no artifact I/O can mask an
/// unaccepted outcome such as a SIGKILL. `Complete` here means only that the
/// run may go on to publication and placement.
fn judge(
    run: &ToolRun,
    artifacts: &Path,
    outputs: &[(String, &Path)],
    ended: crate::process::Ended,
) -> Result<Settled, Error> {
    if !ended
        .code()
        .is_some_and(|c| run.completes(c, &ended.stderr))
    {
        return Ok(Settled::Unaccepted { ended });
    }
    let invalid = invalid_artifacts(
        artifacts,
        outputs.iter().map(|(n, _)| n.as_str()).chain([STDOUT]),
    )?;
    if invalid.is_empty() {
        Ok(Settled::Complete {
            ended,
            cached: false,
        })
    } else {
        Ok(Settled::InvalidOutputs { ended, invalid })
    }
}

/// Place a validated published `entry` and settle it as a complete hit with
/// its stored exit.
fn hit(
    entry: &Path,
    exit_code: i32,
    outputs: &[(String, &Path)],
    sink: &Path,
) -> Result<Settled, Error> {
    use std::os::unix::process::ExitStatusExt;
    place(&entry.join(ARTIFACTS), outputs, sink)?;
    let stderr = fs::read(entry.join(ARTIFACTS).join(STDERR))?;
    // Recency is separate from the immutable published manifest.
    fs::write(entry.join("used"), [])?;
    Ok(Settled::Complete {
        ended: crate::process::Ended {
            status: std::process::ExitStatus::from_raw(exit_code << 8),
            stderr,
        },
        cached: true,
    })
}

impl ReferenceCache {
    /// The persistent summary key of each decode in `runs` (uncached
    /// `osmium cat` runs), all under one observation of the tool's identity,
    /// so keys made together name one binding. `None` under
    /// `--fresh-reference` (summaries are neither read nor written then:
    /// fixture tools answer by file name, not bytes) or when the identity is
    /// unavailable. `reprobe` allows a new probe when the memo no longer
    /// holds; without it a moved binding gives `None`. Each run must name
    /// the output its key promises: osmium picks the OPL writer from `-f`,
    /// never from the output's name, so a launch writing elsewhere decodes
    /// the same bytes.
    pub(crate) fn summary_keys(
        &mut self,
        life: &mut crate::process::Lifecycle,
        watched: &mut crate::run::Run,
        tools: &crate::reference::Tools,
        runs: &[ToolRun],
        reprobe: bool,
    ) -> Result<Option<Vec<Key>>, Error> {
        if self.fresh || runs.is_empty() {
            return Ok(None);
        }
        let mut track = Vec::new();
        let staging = self.staging(&runs[0], &mut track);
        let result = staging.and_then(|staging| {
            identity(
                &mut self.probed,
                life,
                watched,
                &tools.osmium,
                &staging,
                reprobe,
            )
        });
        for dir in &track {
            remove_tree(dir);
        }
        let identity = match result {
            Ok(Some(identity)) => identity,
            Ok(None) => return Ok(None),
            Err(e @ (Error::Interrupted | Error::Unsettled(_) | Error::Environment(_))) => {
                return Err(e);
            }
            Err(e) => {
                output::note(&format!("decoding summaries unavailable: {e}"));
                return Ok(None);
            }
        };
        let mut keys = Vec::with_capacity(runs.len());
        for run in runs {
            let key = key_text(run, &identity, &mut |p| watched.digest_of(p))?;
            let text = format!("pbfhogg-verify-summary {SUMMARY_FORMAT}\n{}", key.text);
            keys.push(Key {
                hex: format!("{:032x}", xxhash_rust::xxh3::xxh3_128(text.as_bytes())),
                text,
            });
        }
        Ok(Some(keys))
    }

    fn summary_path(&self, key: &Key) -> PathBuf {
        self.dir.join(SUMMARIES).join(format!("{}.json", key.hex))
    }

    /// The stored summary under `key`, if one is valid for its full key
    /// text; a malformed or mismatched file is a miss and is removed. A hit
    /// refreshes the file's recency.
    pub(crate) fn summary(&self, key: &Key) -> Option<Summary> {
        if self.fresh {
            return None;
        }
        let path = self.summary_path(key);
        let text = fs::read_to_string(&path).ok()?;
        match serde_json::from_str::<StoredSummary>(&text) {
            Ok(stored) if stored.format == SUMMARY_FORMAT && stored.key == key.text => {
                if let Ok(file) = fs::File::options().append(true).open(&path) {
                    drop(file.set_modified(std::time::SystemTime::now()));
                }
                Some(stored.summary)
            }
            _ => {
                drop(fs::remove_file(&path));
                None
            }
        }
    }

    /// Store `summary` under `key`, replacing any older one. A failure is a
    /// note: a summary saves later work and is never evidence.
    pub(crate) fn publish_summary(&self, key: &Key, summary: &Summary) {
        if self.fresh {
            return;
        }
        let path = self.summary_path(key);
        let result = (|| -> Result<(), Error> {
            fs::create_dir_all(self.dir.join(SUMMARIES))?;
            let tmp = path.with_extension(format!("{}.tmp", self.run_id));
            let text = serde_json::to_string(&StoredSummary {
                format: SUMMARY_FORMAT,
                key: key.text.clone(),
                summary: summary.clone(),
            })
            .map_err(|e| Error::tool(e.to_string()))?;
            fs::write(&tmp, text)?;
            fs::rename(&tmp, &path)?;
            Ok(())
        })();
        if let Err(e) = result {
            output::note(&format!("cannot store a decoding summary: {e}"));
        }
    }

    /// Keep the newest `keep` summaries by recency; zero keeps none.
    fn retain_summaries(&self, keep: usize) {
        let Ok(read) = fs::read_dir(self.dir.join(SUMMARIES)) else {
            return;
        };
        let mut files: Vec<(std::time::SystemTime, PathBuf)> = read
            .flatten()
            .map(|e| {
                let used = e
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::UNIX_EPOCH);
                (used, e.path())
            })
            .collect();
        files.sort_by_key(|f| std::cmp::Reverse(f.0));
        for (_, path) in files.into_iter().skip(keep) {
            drop(fs::remove_file(&path));
        }
    }

    pub(crate) fn execute(
        &mut self,
        life: &mut crate::process::Lifecycle,
        watched: &mut crate::run::Run,
        tools: &crate::reference::Tools,
        run: &ToolRun,
        sink: &Path,
    ) -> Result<Settled, Error> {
        use crate::process::Launch;
        for input in run.inputs() {
            watched.watch(input)?;
        }
        life.checkpoint()?;
        let staging = self.staging(run, &mut Vec::new())?;
        let outputs = run.outputs()?;
        let key = if run.uncached {
            None
        } else {
            cache_key(&mut self.probed, life, watched, tools, &staging, run)?
        };
        watched.revalidate()?;
        if !self.fresh
            && let Some(key) = &key
        {
            let entry = self.dir.join(&key.hex);
            if let Some(manifest) = self.lookup(&entry, key, run, &outputs) {
                life.checkpoint()?;
                let settled = hit(&entry, manifest.exit_code, &outputs, sink)?;
                remove_tree(&staging);
                return Ok(settled);
            }
        }
        let args = staged_args(run, &staging);
        let result = life.run(&Launch {
            program: &tools.osmium,
            args,
            env: crate::reference::run_env(&staging),
            cwd: &staging,
            sink: &staging.join(ARTIFACTS).join(STDOUT),
        });
        // Check even when launching/settling failed; interruption and unsettled
        // trees take priority and can never publish.
        let validation = watched.revalidate();
        let ended = result?;
        validation?;
        let ended = match judge(run, &staging.join(ARTIFACTS), &outputs, ended) {
            Ok(Settled::Complete { ended, .. }) => ended,
            incomplete => {
                remove_tree(&staging);
                return incomplete;
            }
        };
        fs::write(staging.join(ARTIFACTS).join(STDERR), &ended.stderr)?;
        let mut source = staging.join(ARTIFACTS);
        if let Some(key) = key {
            let probed = &mut self.probed;
            let now = identity_text_again(probed, life, watched, tools, &staging, run, true)?;
            watched.revalidate()?;
            if now.as_ref().is_some_and(|now| now.hex == key.hex) {
                life.checkpoint()?;
                let entry = self.dir.join(&key.hex);
                match publish(
                    &staging,
                    &entry,
                    &key.text,
                    ended.code().unwrap_or(0),
                    &outputs,
                    || sealed_checkpoint(probed, life, watched, tools, &staging, run, &key.hex),
                ) {
                    Ok(()) => {
                        source = entry.join(ARTIFACTS);
                    }
                    Err(e @ (Error::Interrupted | Error::Unsettled(_) | Error::Environment(_))) => {
                        return Err(e);
                    }
                    Err(e) => {
                        output::note(&format!("cannot cache reference: {e}"));
                        if !staging.exists() {
                            source = entry.join(ARTIFACTS);
                        }
                    }
                }
            } else {
                output::note("reference identity changed; result not cached");
            }
        }
        place(&source, &outputs, sink)?;
        if staging.exists() {
            remove_tree(&staging);
        }
        Ok(Settled::Complete {
            ended,
            cached: false,
        })
    }

    /// Run uncached reference runs together as one launch group, each with
    /// its standard output in its sink, and settle each in run order. Every
    /// watched input is observed at each member's exit and again once the
    /// group has settled; every member is judged before any is placed, and
    /// a group that did not settle cleanly places nothing. A later member's
    /// failed placement leaves the earlier members placed. Runs that consume
    /// each other's outputs cannot share a group.
    pub(crate) fn execute_uncached_group(
        &mut self,
        life: &mut crate::process::Lifecycle,
        watched: &mut crate::run::Run,
        tools: &crate::reference::Tools,
        runs: &[(&ToolRun, &Path)],
    ) -> Result<Vec<Settled>, Error> {
        let mut stagings = Vec::with_capacity(runs.len());
        let result = self.group_in(life, watched, tools, runs, &mut stagings);
        // A tree that did not settle may still be writing there; the next
        // run's startup sweep removes what stays.
        if matches!(result, Err(Error::Unsettled(_))) {
            return result;
        }
        for staging in &stagings {
            if staging.exists() {
                remove_tree(staging);
            }
        }
        result
    }

    fn group_in(
        &mut self,
        life: &mut crate::process::Lifecycle,
        watched: &mut crate::run::Run,
        tools: &crate::reference::Tools,
        runs: &[(&ToolRun, &Path)],
        stagings: &mut Vec<PathBuf>,
    ) -> Result<Vec<Settled>, Error> {
        use crate::process::Launch;
        for (run, _) in runs {
            if !run.uncached {
                return Err(Error::tool(format!(
                    "reference run {} is cached and cannot join a launch group",
                    run.id
                )));
            }
            for input in run.inputs() {
                watched.watch(input)?;
            }
        }
        life.checkpoint()?;
        let mut planned = Vec::with_capacity(runs.len());
        for (run, _) in runs {
            let staging = self.staging(run, stagings)?;
            planned.push((
                staged_args(run, &staging),
                crate::reference::run_env(&staging),
                staging.join(ARTIFACTS).join(STDOUT),
                run.outputs()?,
            ));
        }
        watched.revalidate()?;
        let launches: Vec<Launch<'_>> = planned
            .iter()
            .zip(stagings.iter())
            .map(|((args, env, sink, _), staging)| Launch {
                program: &tools.osmium,
                args: args.clone(),
                env: env.clone(),
                cwd: staging,
                sink,
            })
            .collect();
        let result = life.run_group(&launches, &mut || watched.revalidate());
        // Check even when launching/settling failed; interruption and
        // unsettled trees take priority.
        let validation = watched.revalidate();
        let ended = result?;
        validation?;
        let mut settled = Vec::with_capacity(runs.len());
        for (((run, _), ended), (staging, (_, _, _, outputs))) in
            runs.iter().zip(ended).zip(stagings.iter().zip(&planned))
        {
            settled.push(judge(run, &staging.join(ARTIFACTS), outputs, ended)?);
        }
        for ((settled, (_, sink)), (staging, (_, _, _, outputs))) in
            settled.iter().zip(runs).zip(stagings.iter().zip(&planned))
        {
            if matches!(settled, Settled::Complete { .. }) {
                life.checkpoint()?;
                place(&staging.join(ARTIFACTS), outputs, sink)?;
            }
        }
        Ok(settled)
    }
}

/// The last check before a sealed staging tree is renamed into place.
/// Sealing reads every artifact in full, so the tool is observed once more,
/// without a reprobe: the staging tree is sealed by now. A moved identity
/// declines publication; input movement and cancellation end the run.
fn sealed_checkpoint(
    probed: &mut Option<Probed>,
    life: &mut crate::process::Lifecycle,
    watched: &mut crate::run::Run,
    tools: &crate::reference::Tools,
    staging: &Path,
    run: &ToolRun,
    hex: &str,
) -> Result<(), Error> {
    let sealed = identity_text_again(probed, life, watched, tools, staging, run, false)?;
    if !sealed.is_some_and(|now| now.hex == hex) {
        return Err(Error::tool("reference identity changed during publication"));
    }
    watched.revalidate()?;
    life.checkpoint()
}

fn cache_key(
    probed: &mut Option<Probed>,
    life: &mut crate::process::Lifecycle,
    watched: &mut crate::run::Run,
    tools: &crate::reference::Tools,
    staging: &Path,
    run: &ToolRun,
) -> Result<Option<Key>, Error> {
    let identity = match identity(probed, life, watched, &tools.osmium, staging, true) {
        Ok(identity) => identity,
        Err(e @ (Error::Interrupted | Error::Unsettled(_) | Error::Environment(_))) => {
            return Err(e);
        }
        Err(e) => {
            output::note(&format!(
                "reference identity unavailable; running uncached: {e}"
            ));
            None
        }
    };
    if identity.is_none() {
        output::note("reference identity unavailable; running uncached");
    }
    identity
        .as_deref()
        .map(|id| key_text(run, id, &mut |p| watched.digest_of(p)))
        .transpose()
}

fn staged_args(run: &ToolRun, staging: &Path) -> Vec<std::ffi::OsString> {
    run.args
        .iter()
        .map(|a| match a {
            RunArg::Lit(s) => s.into(),
            RunArg::Input { prefix, path } => format!("{prefix}{}", path.display()).into(),
            RunArg::Output { prefix, path } => format!(
                "{prefix}{}",
                staging
                    .join(ARTIFACTS)
                    .join(path.file_name().unwrap_or_default())
                    .display()
            )
            .into(),
        })
        .collect()
}

/// The key observed again after a fresh run: the run's probe memo is checked
/// against the executable, each library path's target and each library's
/// digest, and the inputs' digests are rechecked by stamp. The probes run
/// again only when that binding moved and `reprobe` allows it. A library
/// newly placed earlier on the search path, or an `/etc/ld.so.preload`,
/// moves no file the probe named and goes unseen; a local single-user
/// verifier does not defend against that.
fn identity_text_again(
    probed: &mut Option<Probed>,
    life: &mut crate::process::Lifecycle,
    watched: &mut crate::run::Run,
    tools: &crate::reference::Tools,
    staging: &Path,
    run: &ToolRun,
    reprobe: bool,
) -> Result<Option<Key>, Error> {
    identity(probed, life, watched, &tools.osmium, staging, reprobe)?
        .as_deref()
        .map(|id| key_text(run, id, &mut |p| watched.digest_of(p)))
        .transpose()
}

fn key_text(
    run: &ToolRun,
    identity: &str,
    digest: &mut dyn FnMut(&Path) -> Result<String, Error>,
) -> Result<Key, Error> {
    let mut text = format!(
        "pbfhogg-verify-reference {FORMAT}\nrun {}\n{identity}",
        run.id
    );
    for (k, v) in crate::reference::run_env(Path::new("<staging>")) {
        text.push_str(&format!("env {k:?}={v:?}\n"));
    }
    for arg in &run.args {
        let value = match arg {
            RunArg::Lit(s) => format!("literal {s:?}"),
            RunArg::Input { prefix, path } => {
                format!("input {prefix:?} {} {:?}", digest(path)?, extension(path))
            }
            RunArg::Output { prefix, path } => format!("output {prefix:?} {:?}", path.file_name()),
        };
        text.push_str(&format!("arg {value}\n"));
    }
    Ok(Key {
        hex: format!("{:032x}", xxhash_rust::xxh3::xxh3_128(text.as_bytes())),
        text,
    })
}

fn extension(path: &Path) -> String {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    name.find('.')
        .map_or_else(String::new, |i| name[i..].to_owned())
}

/// The tool identity text, from the run's probe memo while it holds, else
/// from a new probe when `reprobe` allows one. `None` is no identity: the
/// probes failed, the executable moved during them, or the memo no longer
/// holds and no reprobe is allowed.
fn identity(
    probed: &mut Option<Probed>,
    life: &mut crate::process::Lifecycle,
    watched: &mut crate::run::Run,
    program: &Path,
    private: &Path,
    reprobe: bool,
) -> Result<Option<String>, Error> {
    if let Some(memo) = probed.as_ref() {
        if memo.holds(watched, program)? {
            return Ok(Some(memo.text()));
        }
        *probed = None;
    }
    if !reprobe {
        return Ok(None);
    }
    let program_digest = watched.digest_of(program)?;
    let Some(version) = probe(life, program, vec!["--version".into()], private)? else {
        return Ok(None);
    };
    let Some(ldd) = ["/usr/bin/ldd", "/bin/ldd"]
        .iter()
        .map(Path::new)
        .find(|p| p.is_file())
    else {
        return Ok(None);
    };
    let Some(libraries) = probe(life, ldd, vec![program.as_os_str().to_owned()], private)? else {
        return Ok(None);
    };
    let Some(paths) = parse_ldd(&libraries) else {
        return Ok(None);
    };
    let mut libs = Vec::with_capacity(paths.len());
    for raw in paths {
        let canonical = fs::canonicalize(&raw)?;
        let hex = watched.digest_of(&canonical)?;
        libs.push((raw, canonical, hex));
    }
    let memo = Probed {
        program: program.to_path_buf(),
        program_digest,
        version,
        libs,
    };
    // The closure is resolved once more after the digests, and the binding
    // must hold across it: an executable or library replaced between the
    // first `ldd` and its digest would otherwise pair one file's answers
    // with another's bytes for the rest of the run.
    let Some(again) = probe(life, ldd, vec![program.as_os_str().to_owned()], private)? else {
        return Ok(None);
    };
    if parse_ldd(&again).as_deref()
        != Some(
            memo.libs
                .iter()
                .map(|l| l.0.clone())
                .collect::<Vec<_>>()
                .as_slice(),
        )
        || !memo.holds(watched, program)?
    {
        return Ok(None);
    }
    let text = memo.text();
    *probed = Some(memo);
    Ok(Some(text))
}

fn probe(
    life: &mut crate::process::Lifecycle,
    program: &Path,
    args: Vec<std::ffi::OsString>,
    private: &Path,
) -> Result<Option<String>, Error> {
    let sink = private.join("probe");
    let result = life.run(&crate::process::Launch {
        program,
        args,
        env: crate::reference::run_env(private),
        cwd: private,
        sink: &sink,
    })?;
    if !result.status.success() {
        return Ok(None);
    }
    // Probe output is small in the supported tool contract, but refuse an
    // unexpectedly large response rather than collecting arbitrary output.
    if fs::metadata(&sink)?.len() > crate::tunables::STDERR_LIMIT as u64 {
        return Ok(None);
    }
    Ok(Some(fs::read_to_string(sink)?))
}

fn parse_ldd(text: &str) -> Option<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.contains("not found") {
            return None;
        }
        let target = match line.split_once("=>") {
            Some((_, rest)) => rest.split_whitespace().next(),
            None => line.split_whitespace().next(),
        };
        if let Some(target) = target.filter(|t| t.starts_with('/')) {
            paths.push(PathBuf::from(target));
        }
    }
    Some(paths)
}

fn remove_tree(path: &Path) {
    let result = match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) => Err(e),
    };
    if let Err(e) = result
        && e.kind() != std::io::ErrorKind::NotFound
    {
        output::note(&format!("cannot remove {}: {e}", path.display()));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    /// An entry `hex` of `size` logical bytes, last used `age` seconds ago.
    fn entry(dir: &Path, hex: char, size: u64, age: u64) -> PathBuf {
        let path = dir.join(hex.to_string().repeat(32));
        fs::create_dir_all(&path).unwrap();
        let manifest = Manifest {
            format: FORMAT,
            key: String::new(),
            exit_code: 0,
            artifacts: vec![Artifact {
                name: "out".into(),
                xxh128: String::new(),
                stamp: Stamp {
                    dev: 0,
                    ino: 0,
                    size,
                    mtime_ns: 0,
                },
            }],
        };
        let file = path.join(MANIFEST);
        fs::write(&file, serde_json::to_string(&manifest).unwrap()).unwrap();
        let used = std::time::SystemTime::now() - std::time::Duration::from_secs(age);
        fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(used)
            .unwrap();
        path
    }

    /// The most recently used entries that fit stay; an older one past the
    /// budget, one larger than the budget and one whose manifest cannot be
    /// read go; no budget keeps everything.
    #[test]
    fn retention_keeps_the_most_recent_entries_that_fit() {
        let scratch = pbfhogg_test_scratch::scratch();
        let root = scratch.path();
        let cache = ReferenceCache {
            dir: root.join("verify-reference"),
            run_id: "test".into(),
            fresh: false,
            probed: None,
            stagings: 0,
        };
        let newest = entry(&cache.dir, 'a', 40, 10);
        let huge = entry(&cache.dir, 'b', 500, 20);
        let older = entry(&cache.dir, 'c', 50, 30);
        let oldest = entry(&cache.dir, 'd', 70, 40);
        let broken = cache.dir.join("e".repeat(32));
        fs::create_dir_all(&broken).unwrap();

        cache.retain(None);
        assert!(broken.exists() && oldest.exists());

        cache.retain(Some(100));
        assert!(newest.exists() && older.exists());
        assert!(!huge.exists() && !oldest.exists() && !broken.exists());

        let empty = entry(&cache.dir, 'f', 0, 5);
        cache.retain(Some(0));
        assert!(!newest.exists() && !older.exists() && !empty.exists());
        // Nothing is left half-evicted.
        assert_eq!(fs::read_dir(&cache.dir).unwrap().count(), 0);
    }

    #[test]
    fn ldd_output_parses_resolved_libraries_and_refuses_a_missing_one() {
        let text = "\tlinux-vdso.so.1 (0x00007ffd)\n\
                    \tlibz.so.1 => /lib/x86_64-linux-gnu/libz.so.1 (0x00007f)\n\
                    \t/lib64/ld-linux-x86-64.so.2 (0x00007f)\n";
        assert_eq!(
            parse_ldd(text).unwrap(),
            vec![
                PathBuf::from("/lib/x86_64-linux-gnu/libz.so.1"),
                PathBuf::from("/lib64/ld-linux-x86-64.so.2")
            ]
        );
        assert!(parse_ldd("\tlibfoo.so => not found\n").is_none());
    }

    #[test]
    fn cancellation_at_publication_leaves_no_entry() {
        let scratch = pbfhogg_test_scratch::scratch();
        let staging = scratch.path().join("staging");
        for sub in [ARTIFACTS, "home", "scratch"] {
            fs::create_dir_all(staging.join(sub)).unwrap();
        }
        for name in [STDOUT, STDERR] {
            fs::write(staging.join(ARTIFACTS).join(name), []).unwrap();
        }
        let entry = scratch.path().join("entry");
        assert!(matches!(
            publish(&staging, &entry, "key", 0, &[], || Err(Error::Interrupted)),
            Err(Error::Interrupted)
        ));
        assert!(!entry.exists());
        assert!(staging.exists());
    }

    #[test]
    fn cleanup_and_lookup_never_follow_symlinks() {
        use std::os::unix::fs::symlink;
        let scratch = pbfhogg_test_scratch::scratch();
        let outside = scratch.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep"), b"keep").unwrap();
        let cache = ReferenceCache::new(scratch.path(), "current".into(), false).unwrap();
        let old = cache.dir.join(".staging-old-1");
        symlink(&outside, &old).unwrap();
        let current = cache.dir.join(".staging-current-1");
        fs::create_dir(&current).unwrap();
        let entry = cache.dir.join("a".repeat(32));
        symlink(&outside, &entry).unwrap();
        let cache = ReferenceCache::new(scratch.path(), "current".into(), false).unwrap();
        assert!(!old.exists());
        assert!(current.exists());
        let run = ToolRun::osmium("t.1");
        let key = Key {
            text: "key".into(),
            hex: "a".repeat(32),
        };
        assert!(cache.lookup(&entry, &key, &run, &[]).is_none());
        assert!(!entry.exists());
        assert_eq!(fs::read(outside.join("keep")).unwrap(), b"keep");
    }

    #[test]
    fn extensions_keep_every_suffix() {
        assert_eq!(extension(Path::new("/d/denmark.osm.pbf")), ".osm.pbf");
        assert_eq!(extension(Path::new("x.osc.gz")), ".osc.gz");
        assert_eq!(extension(Path::new("plain")), "");
    }

    #[test]
    fn the_key_names_inputs_by_content_and_outputs_by_artifact() {
        let scratch = pbfhogg_test_scratch::scratch();
        let root = scratch.path();
        let input = root.join("in.osm.pbf");
        fs::write(&input, b"one").unwrap();
        let run = |input: &Path, out: &str| {
            ToolRun::osmium("sort.1")
                .arg("sort")
                .input(input)
                .arg("-o")
                .output(&root.join("a").join(out))
        };
        let a = key_text(&run(&input, "osmium.osm.pbf"), "tool x\n", &mut digest).unwrap();
        // Another check directory, same artifact name: the same key.
        let moved = ToolRun::osmium("sort.1")
            .arg("sort")
            .input(&input)
            .arg("-o")
            .output(&root.join("b").join("osmium.osm.pbf"));
        assert_eq!(
            a.hex,
            key_text(&moved, "tool x\n", &mut digest).unwrap().hex
        );
        // Another tool identity, another artifact name or other input bytes:
        // another key.
        assert_ne!(
            a.hex,
            key_text(&run(&input, "osmium.osm.pbf"), "tool y\n", &mut digest)
                .unwrap()
                .hex
        );
        assert_ne!(
            a.hex,
            key_text(&run(&input, "other.osm.pbf"), "tool x\n", &mut digest)
                .unwrap()
                .hex
        );
        let other = root.join("other.osm.pbf");
        fs::write(&other, b"two").unwrap();
        assert_ne!(
            a.hex,
            key_text(&run(&other, "osmium.osm.pbf"), "tool x\n", &mut digest)
                .unwrap()
                .hex
        );
        assert!(
            a.text.contains("output \"\" Some(\"osmium.osm.pbf\")"),
            "{}",
            a.text
        );
        assert!(a.text.contains("arg input"), "{}", a.text);
    }

    #[test]
    fn placement_links_into_an_absent_name_and_refuses_an_existing_one() {
        let scratch = pbfhogg_test_scratch::scratch();
        let root = scratch.path();
        let source = root.join("artifacts");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("osmium.osm.pbf"), b"pbf").unwrap();
        fs::write(source.join(STDOUT), b"out").unwrap();
        let check = root.join("check");
        fs::create_dir_all(&check).unwrap();
        let out = check.join("osmium.osm.pbf");
        let outputs = vec![("osmium.osm.pbf".to_owned(), out.as_path())];
        place(&source, &outputs, &check.join("osmium.stdout")).unwrap();
        assert_eq!(fs::read(&out).unwrap(), b"pbf");
        assert_eq!(
            fs::metadata(&out).unwrap().ino(),
            fs::metadata(source.join("osmium.osm.pbf")).unwrap().ino()
        );
        // A second placement over the same names is refused and leaves the
        // first in place.
        assert!(place(&source, &outputs, &check.join("osmium.stdout")).is_err());
        assert!(out.exists());
        // A source that is no longer a regular file is refused, and the
        // names already made are rolled back.
        let fresh = root.join("fresh");
        fs::create_dir_all(&fresh).unwrap();
        let moved = fresh.join("osmium.osm.pbf");
        let outputs = vec![("osmium.osm.pbf".to_owned(), moved.as_path())];
        fs::remove_file(source.join(STDOUT)).unwrap();
        std::os::unix::fs::symlink(source.join("osmium.osm.pbf"), source.join(STDOUT)).unwrap();
        assert!(place(&source, &outputs, &fresh.join("osmium.stdout")).is_err());
        assert!(!moved.exists());
    }

    #[test]
    fn a_published_entry_is_found_and_a_tampered_one_is_dropped() {
        let scratch = pbfhogg_test_scratch::scratch();
        let root = scratch.path();
        let staging = root.join(".staging-1-0");
        for sub in [ARTIFACTS, "home", "scratch"] {
            fs::create_dir_all(staging.join(sub)).unwrap();
        }
        for (name, body) in [("osmium.osm.pbf", "pbf"), (STDOUT, ""), (STDERR, "")] {
            fs::write(staging.join(ARTIFACTS).join(name), body).unwrap();
        }
        let check = root.join("check");
        fs::create_dir_all(&check).unwrap();
        let out = check.join("osmium.osm.pbf");
        let outputs = vec![("osmium.osm.pbf".to_owned(), out.as_path())];
        let run = ToolRun::osmium("t.1").arg("sort").output(&out);
        let key = Key {
            text: "k".into(),
            hex: "00".into(),
        };
        let entry = root.join("00");
        publish(&staging, &entry, &key.text, 0, &outputs, || Ok(())).unwrap();
        assert!(!staging.exists());
        assert!(!entry.join("home").exists());
        let cache = ReferenceCache {
            dir: root.to_path_buf(),
            run_id: "test".into(),
            fresh: false,
            probed: None,
            stagings: 0,
        };
        assert!(cache.lookup(&entry, &key, &run, &outputs).is_some());
        // Linking moves ctime only; the entry stays valid.
        place(&entry.join(ARTIFACTS), &outputs, &check.join("o.stdout")).unwrap();
        assert!(cache.lookup(&entry, &key, &run, &outputs).is_some());
        // The stamp stands in for the bytes: a same-size rewrite in place
        // with mtime set back is trusted by design.
        let artifact = entry.join(ARTIFACTS).join("osmium.osm.pbf");
        let mtime = fs::metadata(&artifact).unwrap().modified().unwrap();
        fs::set_permissions(&artifact, fs::Permissions::from_mode(0o644)).unwrap();
        let file = fs::OpenOptions::new().write(true).open(&artifact).unwrap();
        (&file).write_all(b"PBF").unwrap();
        file.set_modified(mtime).unwrap();
        drop(file);
        assert!(cache.lookup(&entry, &key, &run, &outputs).is_some());
        // Replacing an artifact's bytes moves its stamp; the rehash fails it.
        fs::remove_file(&artifact).unwrap();
        fs::write(&artifact, b"different").unwrap();
        assert!(cache.lookup(&entry, &key, &run, &outputs).is_none());
        assert!(!entry.exists());
        // The check's own link kept the inode it was placed with.
        assert_eq!(fs::read(&out).unwrap(), b"PBF");
    }

    #[test]
    fn a_stored_exit_outside_todays_accepted_set_is_a_miss() {
        let scratch = pbfhogg_test_scratch::scratch();
        let root = scratch.path();
        let staging = root.join(".staging-1-0");
        for sub in [ARTIFACTS, "home", "scratch"] {
            fs::create_dir_all(staging.join(sub)).unwrap();
        }
        for name in [STDOUT, STDERR] {
            fs::write(staging.join(ARTIFACTS).join(name), "").unwrap();
        }
        let entry = root.join("01");
        publish(&staging, &entry, "k", 1, &[], || Ok(())).unwrap();
        let cache = ReferenceCache {
            dir: root.to_path_buf(),
            run_id: "test".into(),
            fresh: false,
            probed: None,
            stagings: 0,
        };
        let key = Key {
            text: "k".into(),
            hex: "01".into(),
        };
        let strict = ToolRun::osmium("t.1").arg("diff");
        assert!(cache.lookup(&entry, &key, &strict, &[]).is_none());
    }

    #[test]
    fn a_conditional_exit_completes_only_with_its_stderr() {
        fn says_done(stderr: &[u8]) -> bool {
            stderr.ends_with(b"Done.\n")
        }
        let run = ToolRun::osmium("t.1").arg("getid").accept_if(1, says_done);
        assert!(run.completes(0, b""));
        assert!(run.completes(1, b"Done.\n"));
        assert!(!run.completes(1, b"Out of memory.\n"));
        assert!(!run.completes(2, b"Done.\n"));

        let scratch = pbfhogg_test_scratch::scratch();
        let root = scratch.path();
        let cache = ReferenceCache {
            dir: root.to_path_buf(),
            run_id: "test".into(),
            fresh: false,
            probed: None,
            stagings: 0,
        };
        for (hex, stderr, found) in [("02", "Done.\n", true), ("03", "boom\n", false)] {
            let staging = root.join(format!(".staging-1-{hex}"));
            for sub in [ARTIFACTS, "home", "scratch"] {
                fs::create_dir_all(staging.join(sub)).unwrap();
            }
            fs::write(staging.join(ARTIFACTS).join(STDOUT), "").unwrap();
            fs::write(staging.join(ARTIFACTS).join(STDERR), stderr).unwrap();
            let entry = root.join(hex);
            publish(&staging, &entry, "k", 1, &[], || Ok(())).unwrap();
            let key = Key {
                text: "k".into(),
                hex: hex.into(),
            };
            assert_eq!(
                cache.lookup(&entry, &key, &run, &[]).is_some(),
                found,
                "{hex}"
            );
            assert_eq!(entry.exists(), found, "{hex}");
        }
    }
}
