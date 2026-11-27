//! A pbfhogg argv together with its effects, built by the same calls.
//!
//! Paths go in through methods that say what they are: an input read, an
//! output the CLI writes (which gets a private `--staging-dir`), and whether
//! the invocation shape writes run scratch (which gets a private
//! `--tmp-dir`). Scratch is shape-dependent in the CLI: it is accepted by
//! `add-locations-to-ways`, `build-geocode-index`, `diff --format osc`, and
//! `merge-changes` with several inputs, no `--simplify` and `--jobs` other
//! than 1; a text `diff` and every other `merge-changes` refuse `--tmp-dir`.
//! The caller who builds such a shape declares it with [`PbfArgv::scratch`].
//!
//! The file a launch's stdout goes to is the harness's sink, not a CLI
//! output, and never triggers staging. The verifier never passes
//! `--overwrite`: every output lands in a fresh check directory.
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::producer::{Arg, IoMode, Key, Request};
use crate::run::{Run, Source, suffix};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Memo {
    Output,
    Stdout,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathArg {
    Input,
    Output,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PbfArgv {
    args: Vec<OsString>,
    inputs: Vec<PathBuf>,
    outputs: Vec<PathBuf>,
    paths: Vec<(usize, PathArg)>,
    scratch: bool,
    memo: Option<Memo>,
}

impl PbfArgv {
    pub(crate) fn new(subcommand: &str) -> Self {
        Self {
            args: vec![subcommand.into()],
            inputs: Vec::new(),
            outputs: Vec::new(),
            paths: Vec::new(),
            scratch: false,
            memo: None,
        }
    }

    /// An argument naming no path.
    pub(crate) fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub(crate) fn arg_if(self, cond: bool, arg: impl Into<OsString>) -> Self {
        if cond { self.arg(arg) } else { self }
    }

    pub(crate) fn args_from(mut self, args: impl IntoIterator<Item = impl Into<OsString>>) -> Self {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Config document and the output/polygon effects produced with it.
    pub(crate) fn extract_config(
        mut self,
        path: &Path,
        config: crate::extract_config::MultiExtractConfig,
    ) -> Self {
        self.args.push("--config".into());
        self.args.push(path.into());
        self.inputs.push(path.to_path_buf());
        self.inputs.extend(config.reads);
        self.outputs.extend(config.outputs);
        self
    }

    pub(crate) fn writes(&self) -> &[PathBuf] {
        &self.outputs
    }

    /// A positional input path.
    pub(crate) fn input(mut self, path: &Path) -> Self {
        self.paths.push((self.args.len(), PathArg::Input));
        self.args.push(path.into());
        self.inputs.push(path.to_path_buf());
        self
    }

    /// `--output PATH`. Long flags only: pbfhogg has reassigned short
    /// letters between versions.
    pub(crate) fn output(self, path: &Path) -> Self {
        self.output_flag("--output", path)
    }

    /// An output named by a command's own long flag (`--output-dir` of
    /// `build-geocode-index`), with the same staging effect as [`Self::output`].
    pub(crate) fn output_flag(mut self, flag: &str, path: &Path) -> Self {
        self.args.push(flag.into());
        self.paths.push((self.args.len(), PathArg::Output));
        self.args.push(path.into());
        self.outputs.push(path.to_path_buf());
        self
    }

    /// This invocation shape writes run scratch and accepts `--tmp-dir`.
    pub(crate) fn scratch(mut self) -> Self {
        self.scratch = true;
        self
    }

    pub(crate) fn subcommand(&self) -> String {
        self.args
            .first()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Share a single file output. Every consumer must treat it as read-only.
    pub(crate) fn memoized_output(mut self) -> Self {
        self.memo = Some(Memo::Output);
        self
    }

    /// Share a report written to stdout. Every consumer reads the report
    /// and judges the recorded exit independently.
    pub(crate) fn memoized_stdout(mut self) -> Self {
        self.memo = Some(Memo::Stdout);
        self
    }

    pub(crate) fn is_memoized(&self) -> bool {
        self.memo.is_some()
    }

    pub(crate) fn request(&self, run: &mut Run, sink: &Path) -> Result<Option<Request>, Error> {
        let Some(memo) = self.memo else {
            return Ok(None);
        };
        let artifact: &Path = match (memo, self.outputs.as_slice()) {
            (Memo::Output, [path]) => path,
            (Memo::Stdout, []) => sink,
            _ => return Err(Error::tool("a memoized producer must declare one artifact")),
        };
        if self.paths.len() != self.inputs.len() + self.outputs.len() {
            return Err(Error::tool("a memoized producer cannot use indirect paths"));
        }
        let mut paths = self.paths.iter().peekable();
        let mut io = IoMode::Buffered;
        let mut args = Vec::with_capacity(self.args.len());
        for (index, arg) in self.args.iter().enumerate() {
            let kind = paths
                .next_if(|(slot, _)| *slot == index)
                .map(|(_, kind)| kind);
            args.push(match kind {
                Some(PathArg::Input) => {
                    let path = Path::new(arg);
                    run.watch(path)?;
                    Arg::Input(Source::of(path)?)
                }
                Some(PathArg::Output) => Arg::Output(suffix(Path::new(arg))),
                None => {
                    if arg == "--direct-io" {
                        io = IoMode::Direct;
                    }
                    Arg::Literal(arg.clone())
                }
            });
        }
        Ok(Some(Request {
            key: Key {
                executable: run.pbfhogg_digest.clone(),
                args,
                io,
                scratch: self.scratch,
            },
            artifact: artifact.to_path_buf(),
        }))
    }

    /// The final argv, with private directories under `private`: `staging`
    /// when the CLI writes an output, `tmp` when the shape writes scratch.
    /// Returns the directories to create.
    pub(crate) fn finish(self, private: &Path) -> (Vec<OsString>, Vec<PathBuf>) {
        let mut args = self.args;
        let mut dirs = Vec::new();
        if !self.outputs.is_empty() {
            let staging = private.join("staging");
            args.push("--staging-dir".into());
            args.push(staging.clone().into());
            dirs.push(staging);
        }
        if self.scratch {
            let tmp = private.join("tmp");
            args.push("--tmp-dir".into());
            args.push(tmp.clone().into());
            dirs.push(tmp);
        }
        (args, dirs)
    }
}

/// Buffered construction of an unindexed comparison input.
pub(crate) fn strip_indexdata(input: &Path, output: &Path) -> PbfArgv {
    PbfArgv::new("degrade")
        .arg("--strip-indexdata")
        .input(input)
        .output(output)
        .memoized_output()
}

/// The common apply invocation. Its caller adds any mode flags before
/// opting into reuse of the read-only result.
pub(crate) fn apply_changes(input: &Path, osc: &Path, output: &Path) -> PbfArgv {
    PbfArgv::new("apply-changes")
        .input(input)
        .input(osc)
        .output(output)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn producer_keys_canonicalize_only_declared_paths() {
        let scratch = pbfhogg_test_scratch::scratch();
        let dir = scratch.path();
        let executable = dir.join("exe");
        std::fs::write(&executable, b"first executable").unwrap();
        let mut run = Run::create(dir, &executable, false).unwrap();
        let input = dir.join("base.osm.pbf");
        let osc = dir.join("changes.osc.gz");
        std::fs::write(&input, b"base").unwrap();
        std::fs::write(&osc, b"changes").unwrap();
        let input_alias = dir.join("alias.osm.pbf");
        let osc_alias = dir.join("alias.osc.gz");
        let wrong_format = dir.join("alias.osc");
        std::fs::hard_link(&input, &input_alias).unwrap();
        std::fs::hard_link(&osc, &osc_alias).unwrap();
        std::fs::hard_link(&osc, &wrong_format).unwrap();
        let other_osc = dir.join("other.osc.gz");
        std::fs::write(&other_osc, b"changes").unwrap();
        let sink = dir.join("stdout");
        let out = dir.join("out.osm.pbf");
        let other_out = dir.join("another.osm.pbf");

        let key = |run: &mut Run, argv: PbfArgv| {
            argv.memoized_output()
                .request(run, &sink)
                .unwrap()
                .unwrap()
                .key
        };
        let buffered = key(&mut run, apply_changes(&input, &osc, &out));
        assert_eq!(
            buffered,
            key(
                &mut run,
                apply_changes(&input_alias, &osc_alias, &other_out)
            )
        );
        assert_ne!(
            buffered,
            key(&mut run, apply_changes(&input, &other_osc, &out))
        );
        assert_ne!(
            buffered,
            key(&mut run, apply_changes(&input, &wrong_format, &out))
        );
        let direct = key(
            &mut run,
            apply_changes(&input, &osc, &out).arg("--direct-io"),
        );
        assert_ne!(buffered, direct);
        assert_eq!(direct.io, IoMode::Direct);
        let literal = PbfArgv::new("apply-changes")
            .input(&input)
            .arg(osc.as_os_str())
            .output(&out);
        assert_ne!(buffered, key(&mut run, literal));
        assert_ne!(
            buffered,
            key(&mut run, apply_changes(&input, &osc, &dir.join("out.pbf")))
        );

        std::fs::write(&executable, b"another executable").unwrap();
        let mut other_run = Run::create(dir, &executable, false).unwrap();
        assert_ne!(
            buffered,
            key(&mut other_run, apply_changes(&input, &osc, &out))
        );
    }

    #[test]
    fn outputs_get_staging_and_declared_scratch_gets_tmp() {
        let (args, dirs) = PbfArgv::new("sort")
            .input(Path::new("/d/in.pbf"))
            .output(Path::new("/c/out.pbf"))
            .finish(Path::new("/c/.launch-1"));
        assert_eq!(
            args,
            [
                "sort",
                "/d/in.pbf",
                "--output",
                "/c/out.pbf",
                "--staging-dir",
                "/c/.launch-1/staging"
            ]
            .map(OsString::from)
        );
        assert_eq!(dirs, [PathBuf::from("/c/.launch-1/staging")]);

        let (args, dirs) = PbfArgv::new("diff")
            .arg("--suppress-common")
            .input(Path::new("a"))
            .input(Path::new("b"))
            .finish(Path::new("/p"));
        assert_eq!(args.len(), 4);
        assert!(dirs.is_empty());

        let (args, dirs) = PbfArgv::new("add-locations-to-ways")
            .input(Path::new("a"))
            .output(Path::new("b"))
            .scratch()
            .finish(Path::new("/p"));
        assert!(args.contains(&OsString::from("--tmp-dir")));
        assert_eq!(dirs.len(), 2);
    }
}
