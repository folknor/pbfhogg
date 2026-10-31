//! What a parsed command line names on disk, stated once per command: the
//! files it reads, the outputs it writes, where its run scratch goes and
//! where else its leftovers may be. The input/output alias refusal, the
//! leftover scan and the run scratch directory are all derived from
//! [`CommandPaths`], so a command's paths are never listed twice.
//!
//! Building the value touches no filesystem: every location is lexical, and
//! the checks that resolve paths run where they always did.

use std::path::{Path, PathBuf};

use super::{Command, DiffFormat, TmpDirArg};

/// An output a command line names.
pub(crate) enum NamedOutput {
    /// A file, published by rename beside itself.
    File(PathBuf),
    /// A geocode index directory, published by swapping the whole directory:
    /// whatever the replaced one held is deleted with it.
    IndexDir(PathBuf),
}

impl NamedOutput {
    fn path(&self) -> &Path {
        match self {
            Self::File(path) | Self::IndexDir(path) => path,
        }
    }
}

/// Where a run that makes scratch puts its run directory.
pub(crate) struct ScratchSite {
    /// The directory the run directory is made in.
    parent: PathBuf,
    /// Whether `parent` came from `--tmp-dir`, which a failure names.
    explicit: bool,
}

impl ScratchSite {
    /// `--tmp-dir`, else the directory of `output` (the file or index
    /// directory the command writes), else the current directory, when the
    /// output name has no directory part or there is no output.
    fn new(tmp: &TmpDirArg, output: Option<&Path>) -> Self {
        match &tmp.tmp_dir {
            Some(dir) => Self {
                parent: dir.clone(),
                explicit: true,
            },
            None => Self {
                parent: output.map_or_else(|| PathBuf::from("."), parent_of),
                explicit: false,
            },
        }
    }

    /// Make the run's scratch directory. Called once the output has been
    /// checked (so a missing output directory reports the output, and a
    /// geocode index directory is judged before scratch could be made inside
    /// it) and before any input is opened, so a missing or unwritable
    /// `--tmp-dir` fails first (ADR-0018).
    pub(crate) fn create(
        &self,
        host: &pbfhogg::host::Host,
    ) -> Result<pbfhogg::scratch::RunScratch, Box<dyn std::error::Error>> {
        let scratch = pbfhogg::scratch::RunScratch::create(&self.parent, host);
        if self.explicit {
            scratch.map_err(|e| {
                format!(
                    "--tmp-dir {}: {}",
                    self.parent.display(),
                    super::error_report(&e)
                )
                .into()
            })
        } else {
            Ok(scratch?)
        }
    }
}

/// Whether a `diff` run writes scratch: `--format osc` always does (its
/// per-action fragments); a text diff never does, at any `-j`, its workers'
/// text going straight to the output. `validate_diff` refuses `--tmp-dir`
/// on a text diff.
fn diff_writes_scratch(format: DiffFormat) -> bool {
    matches!(format, DiffFormat::Osc)
}

/// See the module doc.
pub(crate) struct CommandPaths {
    /// Every file the command reads that the command line names.
    inputs: Vec<PathBuf>,
    /// Every output the command line names. `extract --config` names its
    /// outputs in the config, so they are known, and checked, only once the
    /// config is read.
    outputs: Vec<NamedOutput>,
    /// Where this run's scratch goes; `Some` exactly when the run makes a
    /// run directory.
    scratch: Option<ScratchSite>,
    /// Further directories the leftover scan covers, scanned first: where
    /// older versions of the command, or its other modes, put their scratch.
    also_scan: Vec<PathBuf>,
    /// `--staging-dir`: where the run's output staging goes instead of
    /// beside each output.
    staging: Option<PathBuf>,
}

impl CommandPaths {
    // A new `Command` variant must be considered here, not swallowed by `_`.
    #[deny(clippy::wildcard_enum_match_arm)]
    #[allow(clippy::too_many_lines)]
    pub(crate) fn of(command: &Command) -> Self {
        let mut paths = Self {
            inputs: Vec::new(),
            outputs: Vec::new(),
            scratch: None,
            also_scan: Vec::new(),
            staging: None,
        };
        let file = |path: &PathBuf| NamedOutput::File(path.clone());
        match command {
            Command::Cat { files, output, .. } => {
                paths.inputs.clone_from(files);
                paths.outputs.push(file(&output.output));
                paths.staging.clone_from(&output.staging_dir);
            }
            Command::Sort {
                file: input,
                output,
                ..
            }
            | Command::Repack {
                file: input,
                output,
                ..
            }
            | Command::Degrade {
                file: input,
                output,
                ..
            }
            | Command::Renumber {
                file: input,
                output,
                ..
            }
            | Command::TimeFilter {
                file: input,
                output,
                ..
            } => {
                paths.inputs.push(input.clone());
                paths.outputs.push(file(&output.output));
                paths.staging.clone_from(&output.staging_dir);
            }
            Command::AddLocationsToWays {
                file: input,
                output,
                tmp,
                ..
            } => {
                paths.inputs.push(input.clone());
                paths.outputs.push(file(&output.output));
                paths.staging.clone_from(&output.staging_dir);
                paths.scratch = Some(ScratchSite::new(tmp, Some(&output.output)));
            }
            Command::TagsFilter {
                file: input,
                output,
                expressions_file,
                ..
            } => {
                paths.inputs.push(input.clone());
                paths.inputs.extend(expressions_file.clone());
                paths.outputs.push(file(&output.output));
                paths.staging.clone_from(&output.staging_dir);
            }
            Command::Getid {
                file: input,
                output,
                id_file,
                id_osm_file,
                ..
            }
            | Command::Getparents {
                file: input,
                output,
                id_file,
                id_osm_file,
                ..
            } => {
                paths.inputs.push(input.clone());
                paths.inputs.extend(id_file.clone());
                paths.inputs.extend(id_osm_file.clone());
                paths.outputs.push(file(&output.output));
                paths.staging.clone_from(&output.staging_dir);
            }
            Command::Export {
                file: input,
                output,
                overwrite,
                expressions_file,
                ..
            } => {
                paths.inputs.push(input.clone());
                paths.inputs.extend(expressions_file.clone());
                paths.outputs.extend(output.as_ref().map(file));
                paths.staging.clone_from(&overwrite.staging_dir);
            }
            Command::Diff {
                old,
                new,
                output,
                overwrite,
                format,
                tmp,
                ..
            } => {
                paths.inputs.extend([old.clone(), new.clone()]);
                paths.outputs.extend(output.as_ref().map(file));
                paths.staging.clone_from(&overwrite.staging_dir);
                if diff_writes_scratch(*format) {
                    paths.scratch = Some(ScratchSite::new(tmp, output.as_deref()));
                }
                // Older versions wrote `diff-par-*` shards beside the old
                // input.
                paths.also_scan.push(parent_of(old));
                // Where a stdout diff that made scratch put it; a run that
                // makes none (a text diff) still scans it for an earlier
                // run's leftover.
                if output.is_none() && tmp.tmp_dir.is_none() {
                    paths.also_scan.push(PathBuf::from("."));
                }
            }
            Command::Extract {
                file: input,
                output,
                staging_dir,
                polygon,
                config,
                ..
            } => {
                paths.inputs.push(input.clone());
                paths.inputs.extend(polygon.clone());
                paths.inputs.extend(config.clone());
                paths.outputs.extend(output.as_ref().map(file));
                paths.staging.clone_from(staging_dir);
            }
            Command::ApplyChanges {
                base,
                changes,
                output,
                ..
            }
            | Command::BenchMerge {
                base,
                changes,
                output,
                ..
            } => {
                paths.inputs.extend([base.clone(), changes.clone()]);
                paths.outputs.push(file(&output.output));
                paths.staging.clone_from(&output.staging_dir);
            }
            Command::MergeChanges {
                changes,
                output,
                tmp,
                ..
            } => {
                paths.inputs.clone_from(changes);
                paths.outputs.push(file(&output.output));
                paths.staging.clone_from(&output.staging_dir);
                paths.scratch = Some(ScratchSite::new(tmp, Some(&output.output)));
            }
            Command::BuildGeocodeIndex {
                file: input,
                output_dir,
                tmp,
                staging_dir,
                ..
            } => {
                paths.inputs.push(input.clone());
                paths
                    .outputs
                    .push(NamedOutput::IndexDir(output_dir.clone()));
                paths.staging.clone_from(staging_dir);
                paths.scratch = Some(ScratchSite::new(tmp, Some(output_dir)));
            }
            Command::Inspect {
                subcommand, file, ..
            } => {
                paths.inputs.extend(file.clone());
                if let Some(super::InspectCommand::Tags {
                    file,
                    expressions_file,
                    ..
                }) = subcommand
                {
                    paths.inputs.push(file.clone());
                    paths.inputs.extend(expressions_file.clone());
                }
            }
            Command::Digest { file: input, .. }
            | Command::Check { file: input, .. }
            | Command::BenchRead { file: input, .. }
            | Command::BenchWrite { file: input, .. } => paths.inputs.push(input.clone()),
        }
        paths
    }

    /// Refuse an output that is, or holds, a file the command reads, before
    /// any work: replacing the output would destroy the input.
    pub(crate) fn refuse_aliases(&self) -> Result<(), Box<dyn std::error::Error>> {
        let inputs: Vec<&Path> = self.inputs.iter().map(PathBuf::as_path).collect();
        for output in &self.outputs {
            match output {
                NamedOutput::File(path) => reject_aliases(&inputs, path)?,
                NamedOutput::IndexDir(dir) => {
                    for input in &inputs {
                        if dir_holds(dir, input)? {
                            return Err(format!(
                                "output '{}' contains input '{}', which replacing it would delete",
                                dir.display(),
                                input.display()
                            )
                            .into());
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// The run's scratch directory, made now; see [`ScratchSite::create`].
    ///
    /// # Errors
    /// The command line makes no scratch: the caller asked for scratch on a
    /// command whose arm above gives it none.
    pub(crate) fn create_scratch(
        &self,
        host: &pbfhogg::host::Host,
    ) -> Result<pbfhogg::scratch::RunScratch, Box<dyn std::error::Error>> {
        self.scratch
            .as_ref()
            .ok_or("internal error: this command line makes no scratch directory")?
            .create(host)
    }

    /// `--staging-dir`, when given.
    pub(crate) fn staging_dir(&self) -> Option<&Path> {
        self.staging.as_deref()
    }

    /// Report what earlier runs that never finished left where this command
    /// line's leftovers would be: the places in `also_scan`, the directory of
    /// each output (staging, unless `--staging-dir` moves it, and backups sit
    /// beside it), `--staging-dir`, the scratch location, and the inside of
    /// an index directory about to be replaced.
    pub(crate) fn scan_leftovers(&self, scanner: &mut LeftoverScanner, host: &pbfhogg::host::Host) {
        for dir in &self.also_scan {
            scanner.dir(dir, host);
        }
        for output in &self.outputs {
            scanner.dir(&parent_of(output.path()), host);
        }
        if let Some(dir) = &self.staging {
            scanner.dir(dir, host);
        }
        if let Some(site) = &self.scratch {
            scanner.dir(&site.parent, host);
        }
        for output in &self.outputs {
            if let NamedOutput::IndexDir(dir) = output {
                scanner.index(dir, host);
            }
        }
    }
}

/// Whether replacing the directory `dir` would delete `input`: the file it
/// resolves to lies inside `dir`, or the directory entry that names it does
/// (a symlink inside `dir` is deleted with it even when its target is not).
/// An entry further up the input's path, such as a symlinked directory inside
/// `dir`, only loses that link, not the input or its entry, and is allowed.
/// A missing input or directory holds nothing; the command's own checks say
/// what is wrong with it.
fn dir_holds(dir: &Path, input: &Path) -> std::io::Result<bool> {
    let resolve = |path: &Path| match std::fs::canonicalize(path) {
        Ok(path) => Ok(Some(path)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(std::io::Error::new(
            e.kind(),
            format!("failed to resolve {}: {e}", path.display()),
        )),
    };
    let (Some(dir), Some(target)) = (resolve(dir)?, resolve(input)?) else {
        return Ok(false);
    };
    if target.starts_with(&dir) {
        return Ok(true);
    }
    Ok(planned_path(input)?.is_some_and(|entry| entry.starts_with(&dir)))
}

fn paths_alias(input: &Path, output: &Path) -> std::io::Result<bool> {
    // An input that does not exist cannot alias anything, and reporting it
    // from here would replace the command's own "failed to open <path>"
    // with a bare aliasing-check IO error that never names the file. The
    // check runs before every command, so it must stay silent about
    // problems that are not aliasing and let the command speak first.
    // `same_file` answers false for a missing path on either side.
    //
    // File identity (symlinks, hard links) is the library's test, shared
    // with the library entry points that refuse an aliased output; only the
    // not-yet-created planned-path case below is CLI-specific.
    if pbfhogg::path_guard::same_file(input, output)? {
        return Ok(true);
    }
    if output.exists() {
        return Ok(false);
    }
    let input_canonical = match std::fs::canonicalize(input) {
        Ok(path) => path,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };

    let Some(planned) = planned_path(output)? else {
        return Ok(false);
    };
    Ok(input_canonical == planned)
}

/// Resolve the location a path's final directory entry occupies (for a
/// not-yet-created file, the location it will occupy): the canonical parent
/// directory joined with the final component. Returns `None` when the path
/// has no file name (e.g. a bare `..`), which can never be a valid output.
fn planned_path(path: &Path) -> std::io::Result<Option<PathBuf>> {
    // A bare relative filename has an empty parent, which means the current directory.
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = std::fs::canonicalize(parent)?;
    Ok(path.file_name().map(|name| parent.join(name)))
}

pub(crate) fn reject_aliases(
    inputs: &[&Path],
    output: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    for input in inputs {
        if paths_alias(input, output)? {
            return Err(format!(
                "output '{}' aliases input '{}'",
                output.display(),
                input.display()
            )
            .into());
        }
    }
    Ok(())
}

/// The directory a path sits in, the current directory for a bare name.
pub(crate) fn parent_of(path: &Path) -> PathBuf {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// Reports what earlier runs that never finished left behind, before the run
/// reserves any output of its own. Nothing is removed. Each directory is
/// scanned at most once per kind of scan, however many ways a command reaches
/// it (two spellings, a `--tmp-dir` that is the output's directory, several
/// `extract --config` outputs in one directory).
#[derive(Default)]
pub(crate) struct LeftoverScanner {
    /// Directories already scanned beside outputs and for scratch.
    beside: std::collections::HashSet<(u64, u64)>,
    /// Directories already scanned from the inside, as an index to replace.
    inside_index: std::collections::HashSet<(u64, u64)>,
}

impl LeftoverScanner {
    /// `true` the first time `dir` is seen in `seen`. A directory that does
    /// not exist has no leftovers; the command's own checks say what is wrong
    /// with it.
    fn first_visit(seen: &mut std::collections::HashSet<(u64, u64)>, dir: &Path) -> bool {
        use std::os::unix::fs::MetadataExt as _;
        std::fs::metadata(dir).is_ok_and(|meta| seen.insert((meta.dev(), meta.ino())))
    }

    pub(crate) fn dir(&mut self, dir: &Path, host: &pbfhogg::host::Host) {
        if Self::first_visit(&mut self.beside, dir) {
            let scan = pbfhogg::scratch::find_stale(dir);
            pbfhogg::scratch::report_stale(&host.diagnostics, &scan);
        }
    }

    /// An existing index directory about to be replaced.
    fn index(&mut self, dir: &Path, host: &pbfhogg::host::Host) {
        if Self::first_visit(&mut self.inside_index, dir) {
            let scan = pbfhogg::scratch::find_stale_in_index(dir);
            pbfhogg::scratch::report_stale(&host.diagnostics, &scan);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_methods)]
mod tests {
    use super::{ScratchSite, TmpDirArg, dir_holds};
    use crate::host;

    #[test]
    fn scratch_goes_to_tmp_dir_else_the_output_directory_else_dot() {
        let dir = crate::test_scratch::scratch();
        let tmp = dir.path().join("tmp");
        let out_dir = dir.path().join("out");
        std::fs::create_dir(&tmp).unwrap();
        std::fs::create_dir(&out_dir).unwrap();
        let output = out_dir.join("result.pbf");
        let host = host::cli_host(pbfhogg::host::Invocation::new());

        let explicit = TmpDirArg {
            tmp_dir: Some(tmp.clone()),
        };
        let scratch = ScratchSite::new(&explicit, Some(&output))
            .create(&host)
            .unwrap();
        assert_eq!(scratch.path().parent(), Some(tmp.as_path()));
        // Made at startup, not at first use, and the only entry there.
        assert!(scratch.path().is_dir());
        assert_eq!(std::fs::read_dir(&tmp).unwrap().count(), 1);
        drop(scratch);
        assert!(std::fs::read_dir(&tmp).unwrap().next().is_none());

        let default = TmpDirArg { tmp_dir: None };
        let scratch = ScratchSite::new(&default, Some(&output))
            .create(&host)
            .unwrap();
        assert_eq!(scratch.path().parent(), Some(out_dir.as_path()));
        assert!(scratch.path().is_dir());
        assert_eq!(std::fs::read_dir(&out_dir).unwrap().count(), 1);
        drop(scratch);
        assert!(std::fs::read_dir(&out_dir).unwrap().next().is_none());
    }

    /// An output name with no directory part, and no output at all, both
    /// place scratch in the current directory.
    #[test]
    fn a_bare_output_name_or_none_places_scratch_in_the_current_directory() {
        let host = host::cli_host(pbfhogg::host::Invocation::new());
        let default = TmpDirArg { tmp_dir: None };
        for output in [Some(std::path::Path::new("out.pbf")), None] {
            let scratch = ScratchSite::new(&default, output).create(&host).unwrap();
            assert_eq!(scratch.path().parent(), Some(std::path::Path::new(".")));
            assert!(scratch.path().is_dir());
        }
    }

    #[test]
    fn an_explicit_tmp_dir_error_names_the_flag_and_the_directory() {
        let dir = crate::test_scratch::scratch();
        let missing = dir.path().join("missing");
        let host = host::cli_host(pbfhogg::host::Invocation::new());
        let tmp = TmpDirArg {
            tmp_dir: Some(missing.clone()),
        };
        let err = ScratchSite::new(&tmp, None)
            .create(&host)
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("--tmp-dir"), "{err}");
        assert!(err.contains(&missing.display().to_string()), "{err}");
    }

    /// Replacing a directory deletes an input inside it, and the entry that
    /// names an input even when that entry is a symlink or hard link to a
    /// file outside. A path that only passes through the directory, or
    /// through a symlinked directory inside it, loses nothing the run reads.
    #[cfg(unix)]
    #[test]
    fn a_directory_holds_an_input_inside_it_or_the_entry_naming_it() {
        let root = crate::test_scratch::scratch();
        let idx = root.path().join("idx");
        let outside = root.path().join("outside");
        std::fs::create_dir(&idx).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let source = outside.join("in.osm.pbf");
        std::fs::write(&source, b"pbf").unwrap();
        let inside = idx.join("in.osm.pbf");
        std::fs::write(&inside, b"pbf").unwrap();
        let link_in = idx.join("link.osm.pbf");
        std::os::unix::fs::symlink(&source, &link_in).unwrap();
        let link_out = outside.join("into-idx.osm.pbf");
        std::os::unix::fs::symlink(&inside, &link_out).unwrap();
        let dir_link = idx.join("elsewhere");
        std::os::unix::fs::symlink(&outside, &dir_link).unwrap();
        let hard = idx.join("hard.osm.pbf");
        std::fs::hard_link(&source, &hard).unwrap();

        assert!(dir_holds(&idx, &inside).unwrap());
        assert!(dir_holds(&idx, &link_in).unwrap());
        assert!(dir_holds(&idx, &link_out).unwrap());
        assert!(!dir_holds(&idx, &source).unwrap());
        assert!(!dir_holds(&idx, &idx.join("..").join("outside").join("in.osm.pbf")).unwrap());
        assert!(!dir_holds(&idx, &dir_link.join("in.osm.pbf")).unwrap());
        assert!(!dir_holds(&idx, &outside.join("missing.osm.pbf")).unwrap());
        assert!(!dir_holds(&root.path().join("no-such-index"), &source).unwrap());
        // A hard link inside is an entry inside; the outside name for the
        // same inode (asserted above) is not.
        assert!(dir_holds(&idx, &hard).unwrap());
    }
}
