use std::error::Error as StdError;
use std::fmt;
use std::io;

/// Error type for the dev tool.
///
/// No `Box` wrapping — this is a binary, not a library. Keeping it simple.
#[derive(Debug)]
#[allow(dead_code)]
pub enum DevError {
    /// An I/O error.
    Io(io::Error),
    /// Configuration file parse or validation error.
    Config(String),
    /// Build failure (cargo returned non-zero, or compilation error).
    Build(String),
    /// One or more preflight checks failed.
    Preflight(Vec<String>),
    /// A subprocess exited with an error.
    Subprocess {
        program: String,
        code: Option<i32>,
        stderr: String,
    },
    /// Lock file conflict (another dev instance is running).
    Lock(String),
    /// Database error (SQLite operations).
    Database(String),
}

impl fmt::Display for DevError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DevError::Io(err) => write!(f, "io: {err}"),
            DevError::Config(msg) => write!(f, "config: {msg}"),
            DevError::Build(msg) => write!(f, "build: {msg}"),
            DevError::Preflight(failures) => {
                write!(f, "preflight failed:")?;
                for failure in failures {
                    write!(f, "\n  - {failure}")?;
                }
                Ok(())
            }
            DevError::Subprocess {
                program,
                code,
                stderr,
            } => {
                write!(f, "{program}")?;
                match code {
                    Some(c) => write!(f, " exited with code {c}")?,
                    None => write!(f, " killed by signal")?,
                }
                if !stderr.is_empty() {
                    write!(f, ": {stderr}")?;
                }
                Ok(())
            }
            DevError::Lock(msg) => write!(f, "lock: {msg}"),
            DevError::Database(msg) => write!(f, "database: {msg}"),
        }
    }
}

impl StdError for DevError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            DevError::Io(err) => Some(err),
            DevError::Config(_)
            | DevError::Build(_)
            | DevError::Preflight(_)
            | DevError::Subprocess { .. }
            | DevError::Lock(_)
            | DevError::Database(_) => None,
        }
    }
}

impl From<io::Error> for DevError {
    fn from(err: io::Error) -> DevError {
        DevError::Io(err)
    }
}

impl From<toml::de::Error> for DevError {
    fn from(err: toml::de::Error) -> DevError {
        DevError::Config(err.to_string())
    }
}

impl From<serde_json::Error> for DevError {
    fn from(err: serde_json::Error) -> DevError {
        DevError::Config(format!("json: {err}"))
    }
}

impl From<rusqlite::Error> for DevError {
    fn from(err: rusqlite::Error) -> DevError {
        DevError::Database(err.to_string())
    }
}
