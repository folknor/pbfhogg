//! Declared reference-tool runs.
//!
//! A run is a [`ToolRun`]: an id (unique per check and run, versioned by the
//! caller when its meaning changes), a program, literal arguments with its
//! inputs and outputs marked, and the exit codes that complete it. The
//! declaration is what the reference cache keys on; only completed reference
//! executions, and summaries of osmium's OPL decodings, are reused. Every
//! pbfhogg comparison is still decided, from summaries or by a merge.
//!
//! A run starts from a controlled environment, not the caller's: a fixed
//! `PATH` for what a launcher script runs, a C UTF-8 locale, UTC, and a
//! `HOME` and `TMPDIR` private to the launch.
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The `PATH` a reference run sees. Tools are launched by absolute path;
/// this is for what a launcher script runs.
const RUN_PATH: &str = "/usr/bin:/bin";

/// The reference program, resolved at the executable boundary.
#[derive(Debug, Clone)]
pub(crate) struct Tools {
    pub(crate) osmium: PathBuf,
}

use crate::error::Error;
pub(crate) use crate::pbf_frames::Origin;
const MANIFEST: &str = "manifest.json";
const STDOUT: &str = "stdout";
const STDERR: &str = "stderr";
pub(crate) enum RunArg {
    Lit(String),
    /// An input and who wrote it, as the run consumes it: an unchanged copy
    /// of a pbfhogg output is still `Pbfhogg`, an osmium rewrite of one is
    /// `Other`. The origin decides how a failed run is judged and is not
    /// part of the cache key.
    Input {
        prefix: &'static str,
        path: PathBuf,
        origin: Origin,
    },
    Output {
        prefix: &'static str,
        path: PathBuf,
    },
}

/// What a run's stderr must show for an exit code accepted only on that
/// condition to complete it.
pub(crate) type StderrShows = fn(&[u8]) -> bool;

/// One declared osmium run: its id (unique per check and run, and
/// versioned by the caller when the run's meaning changes), its
/// argv with inputs and outputs marked, and the outcomes that complete it:
/// the accepted exit codes, some only when the stderr shows a condition.
pub(crate) struct ToolRun {
    pub(crate) id: String,
    pub(crate) args: Vec<RunArg>,
    pub(crate) accepted: Vec<i32>,
    conditions: Vec<(i32, StderrShows)>,
    /// Never looked up in or published to the reference cache.
    pub(crate) uncached: bool,
}

impl ToolRun {
    pub(crate) fn osmium(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            args: Vec::new(),
            accepted: vec![0],
            conditions: Vec::new(),
            uncached: false,
        }
    }

    /// Run without the reference cache: for a decoding whose output is as
    /// large as its input and read once, which publishing would only move
    /// into the cache until the budget evicts it at the end of the run.
    pub(crate) fn uncached(mut self) -> Self {
        self.uncached = true;
        self
    }

    pub(crate) fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(RunArg::Lit(arg.into()));
        self
    }

    pub(crate) fn args(mut self, args: &[&str]) -> Self {
        self.args
            .extend(args.iter().map(|&a| RunArg::Lit(a.to_owned())));
        self
    }

    /// An input path and who wrote it, passed as its own argument.
    pub(crate) fn input(self, path: &Path, origin: Origin) -> Self {
        self.input_as("", path, origin)
    }

    /// An input path embedded after `prefix` in one argument (`file=`).
    pub(crate) fn input_as(mut self, prefix: &'static str, path: &Path, origin: Origin) -> Self {
        self.args.push(RunArg::Input {
            prefix,
            path: path.to_path_buf(),
            origin,
        });
        self
    }

    /// An output the run writes, at the path the check reads it from. Its
    /// file name is its artifact name.
    pub(crate) fn output(self, path: &Path) -> Self {
        self.output_as("", path)
    }

    /// An output embedded after `prefix` in one argument (`-o=`).
    pub(crate) fn output_as(mut self, prefix: &'static str, path: &Path) -> Self {
        self.args.push(RunArg::Output {
            prefix,
            path: path.to_path_buf(),
        });
        self
    }

    /// Exit `code` completes the run only when `shows` holds for its stderr:
    /// for a tool whose exit code also means something besides failure, such
    /// as osmium getid's exit 1, which is both "an object was not found" and
    /// "an exception ended the run". The condition is checked on a fresh run
    /// before it is cached and on a stored entry's stderr before it is
    /// replayed, so a run that did not complete is never published or reused.
    pub(crate) fn accept_if(mut self, code: i32, shows: StderrShows) -> Self {
        self.accepted.push(code);
        self.conditions.push((code, shows));
        self
    }

    /// Whether exit `code` with `stderr` completes the run.
    pub(crate) fn completes(&self, code: i32, stderr: &[u8]) -> bool {
        self.accepted.contains(&code)
            && self
                .conditions
                .iter()
                .filter(|(c, _)| *c == code)
                .all(|(_, shows)| shows(stderr))
    }

    /// Whether exit `code` completes the run only on a stderr condition.
    pub(crate) fn conditional(&self, code: i32) -> bool {
        self.conditions.iter().any(|(c, _)| *c == code)
    }

    /// The step label: `osmium` and its first literal argument.
    pub(crate) fn label(&self) -> String {
        let first = self.args.iter().find_map(|a| match a {
            RunArg::Lit(s) if !s.starts_with('-') && !s.contains('=') => Some(s.as_str()),
            _ => None,
        });
        match first {
            Some(first) => format!("osmium {first}"),
            None => "osmium".to_owned(),
        }
    }

    pub(crate) fn inputs(&self) -> Vec<&Path> {
        self.args
            .iter()
            .filter_map(|a| match a {
                RunArg::Input { path, .. } => Some(path.as_path()),
                _ => None,
            })
            .collect()
    }

    /// Whether any input is bytes a pbfhogg run wrote.
    pub(crate) fn reads_pbfhogg(&self) -> bool {
        self.args.iter().any(|a| {
            matches!(
                a,
                RunArg::Input {
                    origin: Origin::Pbfhogg,
                    ..
                }
            )
        })
    }

    /// Each output's artifact name and the path the check reads it from.
    pub(crate) fn outputs(&self) -> Result<Vec<(String, &Path)>, Error> {
        let mut out: Vec<(String, &Path)> = Vec::new();
        for arg in &self.args {
            let RunArg::Output { path, .. } = arg else {
                continue;
            };
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .filter(|n| ![MANIFEST, STDOUT, STDERR].contains(n) && !n.starts_with('.'))
                .ok_or_else(|| {
                    Error::tool(format!(
                        "reference output {} has no usable artifact name",
                        path.display()
                    ))
                })?
                .to_owned();
            if out.iter().any(|(n, _)| *n == name) {
                return Err(Error::tool(format!(
                    "reference run {} names artifact {name} twice",
                    self.id
                )));
            }
            out.push((name, path));
        }
        Ok(out)
    }
}

/// The environment of a reference launch whose private directory is `private`.
pub(crate) fn run_env(private: &Path) -> Vec<(OsString, OsString)> {
    vec![
        ("PATH".into(), RUN_PATH.into()),
        ("LANG".into(), "C.UTF-8".into()),
        ("LC_ALL".into(), "C.UTF-8".into()),
        ("TZ".into(), "UTC".into()),
        ("HOME".into(), private.join("home").into_os_string()),
        ("TMPDIR".into(), private.join("scratch").into_os_string()),
    ]
}
