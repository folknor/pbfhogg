//! Declared reference-tool runs.
//!
//! A run is a [`ToolRun`]: an id (unique per check and run, versioned by the
//! caller when its meaning changes), a program, literal arguments with its
//! inputs and outputs marked, and the exit codes that complete it. The
//! declaration is what the reference cache keys on; only completed reference
//! executions are reused. Every pbfhogg comparison still runs.
//!
//! A run starts from a controlled environment, not the caller's: a fixed
//! `PATH` for what a launcher script runs, a C UTF-8 locale, UTC, and a
//! `HOME` and `TMPDIR` private to the launch.
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The `PATH` a reference run sees. Tools are launched by absolute path;
/// this is for what a launcher script runs.
const RUN_PATH: &str = "/usr/bin:/bin";

/// The reference programs, resolved at the executable boundary.
#[derive(Debug, Clone)]
pub(crate) struct Tools {
    pub(crate) osmium: PathBuf,
    pub(crate) osmosis: Option<OsmosisTools>,
    pub(crate) osmconvert: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub(crate) struct OsmosisTools {
    pub(crate) osmosis: PathBuf,
    pub(crate) java_home: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Program {
    Osmium,
    Osmosis,
    Osmconvert,
}

impl Program {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Osmium => "osmium",
            Self::Osmosis => "osmosis",
            Self::Osmconvert => "osmconvert",
        }
    }

    pub(crate) fn path(self, tools: &Tools) -> &Path {
        match self {
            Self::Osmium => &tools.osmium,
            Self::Osmosis => tools.osmosis.as_ref().map_or(Path::new(""), |t| &t.osmosis),
            Self::Osmconvert => tools.osmconvert.as_deref().unwrap_or(Path::new("")),
        }
    }
}

use crate::error::Error;
const MANIFEST: &str = "manifest.json";
const STDOUT: &str = "stdout";
const STDERR: &str = "stderr";
pub(crate) enum RunArg {
    Lit(String),
    Input { prefix: &'static str, path: PathBuf },
    Output { prefix: &'static str, path: PathBuf },
}

/// What a run's stderr must show for an exit code accepted only on that
/// condition to complete it.
pub(crate) type StderrShows = fn(&[u8]) -> bool;

/// One declared reference run: its id (unique per check and run, and
/// versioned by the caller when the run's meaning changes), its program, its
/// argv with inputs and outputs marked, and the outcomes that complete it:
/// the accepted exit codes, some only when the stderr shows a condition.
pub(crate) struct ToolRun {
    pub(crate) id: String,
    pub(crate) program: Program,
    pub(crate) args: Vec<RunArg>,
    pub(crate) accepted: Vec<i32>,
    conditions: Vec<(i32, StderrShows)>,
}

impl ToolRun {
    pub(crate) fn new(id: impl Into<String>, program: Program) -> Self {
        Self {
            id: id.into(),
            program,
            args: Vec::new(),
            accepted: vec![0],
            conditions: Vec::new(),
        }
    }

    pub(crate) fn osmium(id: impl Into<String>) -> Self {
        Self::new(id, Program::Osmium)
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

    /// An input path, passed as its own argument.
    pub(crate) fn input(self, path: &Path) -> Self {
        self.input_as("", path)
    }

    /// An input path embedded after `prefix` in one argument (`file=`).
    pub(crate) fn input_as(mut self, prefix: &'static str, path: &Path) -> Self {
        self.args.push(RunArg::Input {
            prefix,
            path: path.to_path_buf(),
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

    /// Exit codes beside 0 that complete the run (osmium diff and check-refs
    /// exit 1 for a found difference).
    pub(crate) fn accept(mut self, codes: &[i32]) -> Self {
        self.accepted.extend_from_slice(codes);
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

    /// The step label: the program and its first literal argument.
    pub(crate) fn label(&self) -> String {
        let first = self.args.iter().find_map(|a| match a {
            RunArg::Lit(s) if !s.starts_with('-') && !s.contains('=') => Some(s.as_str()),
            _ => None,
        });
        match (&self.program, first) {
            (_, Some(first)) => format!("{} {first}", self.program.name()),
            _ => self.program.name().to_owned(),
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

/// Java properties are passed through the launcher's xargs parser.
pub(crate) fn xargs_safe(path: &Path) -> bool {
    !path
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .any(|b| b.is_ascii_whitespace() || b"\"'\\".contains(b))
}

pub(crate) fn program_env(
    program: Program,
    tools: &Tools,
    private: &Path,
) -> Vec<(OsString, OsString)> {
    let mut env = run_env(private);
    if program == Program::Osmosis
        && let Some(tool) = &tools.osmosis
    {
        env.push(("JAVA_HOME".into(), tool.java_home.clone().into_os_string()));
        if xargs_safe(private) {
            env.push((
                "JAVA_OPTS".into(),
                format!(
                    "-Duser.home={}/home -Djava.io.tmpdir={}/scratch",
                    private.display(),
                    private.display()
                )
                .into(),
            ));
        }
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osmosis_environment_is_private_and_xargs_aware() {
        let tools = Tools {
            osmium: "/osmium".into(),
            osmconvert: None,
            osmosis: Some(OsmosisTools {
                osmosis: "/tool/bin/osmosis".into(),
                java_home: "/tool/jdk".into(),
            }),
        };
        let env = program_env(Program::Osmosis, &tools, Path::new("/private"));
        assert!(env.contains(&("JAVA_HOME".into(), "/tool/jdk".into())));
        assert!(env.contains(&(
            "JAVA_OPTS".into(),
            "-Duser.home=/private/home -Djava.io.tmpdir=/private/scratch".into()
        )));
        for bad in ["/space here", "/quote'", "/quote\"", "/slash\\", "/tab	"] {
            assert!(!xargs_safe(Path::new(bad)));
            assert!(
                !program_env(Program::Osmosis, &tools, Path::new(bad))
                    .iter()
                    .any(|(k, _)| k == "JAVA_OPTS")
            );
        }
        assert!(
            !program_env(Program::Osmium, &tools, Path::new("/private"))
                .iter()
                .any(|(k, _)| k == "JAVA_HOME")
        );
    }
}
