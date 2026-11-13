//! Shared, pbfhogg-independent support for repository development tools.
//!
//! Configuration is supplied explicitly: this crate never reads the process
//! environment or discovers the host. Callers select the host and user config.
mod compose;
mod datasets;
mod registry;
mod selection;
mod tunables;

pub use compose::{Config, user_config_path};
pub use datasets::{ResolvedInput, file_digest};
pub use registry::{DatasetEdit, DatasetView, FileEntry, Generation, replace_file};
pub use selection::Bbox;

/// A configuration, input selection or file verification failure.
#[derive(Debug)]
pub struct Error(String);

impl Error {
    pub(crate) fn message(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod registry_tests;
