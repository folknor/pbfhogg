//! The sole external reference-tool spawn policy for integration tests.
use super::process::{OwnedChild, isolate};
use pbfhogg_test_scratch::TestScratch;
use std::{
    ffi::OsStr,
    io,
    path::PathBuf,
    process::{Command, ExitStatus, Output, Stdio},
};

fn executable() -> io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    for directory in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        // Resolve relative PATH entries against the parent's cwd deliberately,
        // before assigning the child's private working directory.
        let path = std::env::current_dir()?.join(directory).join("osmium");
        match std::fs::metadata(&path) {
            Ok(meta) if meta.is_file() && meta.permissions().mode() & 0o111 != 0 => {
                return path.canonicalize();
            }
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("osmium is not executable: {}", path.display()),
                ));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "osmium not found in PATH",
    ))
}

pub struct Oracle<'a> {
    scratch: &'a TestScratch,
    command: io::Result<Command>,
}

impl<'a> Oracle<'a> {
    pub fn new(scratch: &'a TestScratch) -> Self {
        let command = executable().map(|path| {
            let mut cmd = Command::new(path);
            isolate(&mut cmd, scratch);
            cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
            cmd
        });
        Self { scratch, command }
    }
    pub fn arg(&mut self, value: impl AsRef<OsStr>) -> &mut Self {
        if let Ok(cmd) = &mut self.command {
            cmd.arg(value);
        }
        self
    }
    pub fn args<I, S>(&mut self, values: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        if let Ok(cmd) = &mut self.command {
            cmd.args(values);
        }
        self
    }
    pub fn output(&mut self) -> io::Result<Output> {
        let cmd = self
            .command
            .as_mut()
            .map_err(|e| io::Error::new(e.kind(), e.to_string()))?;
        let mut child = OwnedChild::spawn(cmd, self.scratch)?;
        let out = child.wait(pbfhogg::TEST_CHILD_TIMEOUT)?;
        if cmd.get_args().any(|arg| arg == "--version") {
            assert!(out.status.success(), "osmium --version failed: {out:?}");
            eprintln!(
                "oracle {}: {}",
                cmd.get_program().to_string_lossy(),
                String::from_utf8_lossy(&out.stdout).trim()
            );
        }
        Ok(out)
    }
    pub fn status(&mut self) -> io::Result<ExitStatus> {
        let out = self.output()?;
        assert!(
            out.status.success(),
            "osmium failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(out.status)
    }
}
