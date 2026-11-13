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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PbfArgv {
    args: Vec<OsString>,
    inputs: Vec<PathBuf>,
    outputs: Vec<PathBuf>,
    scratch: bool,
}

impl PbfArgv {
    pub(crate) fn new(subcommand: &str) -> Self {
        Self {
            args: vec![subcommand.into()],
            inputs: Vec::new(),
            outputs: Vec::new(),
            scratch: false,
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
        self.args.push(path.into());
        self.inputs.push(path.to_path_buf());
        self
    }

    /// `--output PATH`. Long flags only: pbfhogg has reassigned short
    /// letters between versions.
    pub(crate) fn output(mut self, path: &Path) -> Self {
        self.args.push("--output".into());
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

#[cfg(test)]
mod tests {
    use super::*;

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
