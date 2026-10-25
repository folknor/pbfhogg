//! Output-side options shared by every command that writes a PBF.
//!
//! [`OutputBackend`] names which writer a run builds; [`WriteOptions`] is the
//! bundle library entry points take instead of the positional
//! `compression, direct_io, io_uring, force` tail they used to carry.

use crate::write::compression::Compression;

/// Which output writer a run builds. Chosen once per run, from the flags,
/// before any blob is written.
///
/// The variants are named after the `PbfWriter` constructors they select:
///
/// | Variant | Constructor | Feature |
/// |---|---|---|
/// | `Buffered` | `PbfWriter::to_path` | none |
/// | `Parallel` | `PbfWriter::to_path_parallel` | none |
/// | `Direct` | `PbfWriter::to_path_direct` | `linux-direct-io` |
/// | `Uring` | `PbfWriter::to_path_uring` | `linux-io-uring` |
///
/// An operator cannot ask for `Parallel`: it is the per-command default for
/// commands whose output pass is writer-bound, applied with
/// [`OutputBackend::parallel_by_default`] after the flags resolved.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum OutputBackend {
    /// Single writer thread over a buffered file.
    #[default]
    Buffered,
    /// Pool of `pwrite` workers; lifts the single-thread write ceiling.
    Parallel,
    /// `O_DIRECT` writes (`linux-direct-io`).
    Direct,
    /// `io_uring` writes with registered buffers (`linux-io-uring`).
    Uring,
}

impl OutputBackend {
    /// Resolve the `--direct-io` / `--io-uring` flag pair. Passing both
    /// resolves to [`OutputBackend::Uring`]: io_uring wins, as it always did.
    #[must_use]
    pub const fn from_flags(direct_io: bool, io_uring: bool) -> Self {
        if io_uring {
            Self::Uring
        } else if direct_io {
            Self::Direct
        } else {
            Self::Buffered
        }
    }

    /// For a command whose output pass prefers the parallel writer: swap the
    /// default [`Buffered`](OutputBackend::Buffered) backend for
    /// [`Parallel`](OutputBackend::Parallel), leaving an explicit `Direct` or
    /// `Uring` choice alone.
    #[must_use]
    pub const fn parallel_by_default(self) -> Self {
        match self {
            Self::Buffered => Self::Parallel,
            other => other,
        }
    }

    /// Whether this build can construct the backend at all: `Direct` needs
    /// the `linux-direct-io` feature, `Uring` the `linux-io-uring` feature.
    /// A front end checks this at its argument boundary so a request for a
    /// backend the build lacks is refused before any input is opened.
    /// (`Direct` also stands for direct *reads*, which need the same feature.)
    #[must_use]
    pub const fn is_compiled(self) -> bool {
        match self {
            Self::Buffered | Self::Parallel => true,
            Self::Direct => cfg!(feature = "linux-direct-io"),
            Self::Uring => cfg!(feature = "linux-io-uring"),
        }
    }

    /// Whether `PbfWriter::write_raw_copy` (a kernel-space `copy_file_range`
    /// splice of an input byte range into the output) is available.
    ///
    /// True iff the backend writes through a regular file descriptor:
    /// `Buffered`, `Parallel` or `Uring`, in every build (the splice used to
    /// need the `linux-direct-io` feature, though it has nothing to do with
    /// `O_DIRECT`). `Direct` is excluded because splicing would bypass its
    /// page-aligned buffering.
    #[must_use]
    pub const fn supports_copy_range(self) -> bool {
        matches!(self, Self::Buffered | Self::Parallel | Self::Uring)
    }
}

/// Output options taken by library entry points that write a PBF.
#[derive(Clone, Copy, Debug)]
pub struct WriteOptions {
    /// Blob compression for the output.
    pub compression: Compression,
    /// Which writer to build.
    pub backend: OutputBackend,
    /// Read the input with `O_DIRECT` (the `--direct-io` flag). Independent of
    /// [`backend`](WriteOptions::backend): `--direct-io --io-uring` resolves
    /// the backend to `Uring` yet still reads directly.
    pub direct_reads: bool,
    /// Proceed on input lacking blob-level indexdata, where the command
    /// would otherwise refuse (the `--force` flag).
    pub force: bool,
}

impl WriteOptions {
    /// Build from the CLI flag set. See [`OutputBackend::from_flags`].
    #[must_use]
    pub const fn from_flags(
        compression: Compression,
        direct_io: bool,
        io_uring: bool,
        force: bool,
    ) -> Self {
        Self {
            compression,
            backend: OutputBackend::from_flags(direct_io, io_uring),
            direct_reads: direct_io,
            force,
        }
    }
}

impl Default for WriteOptions {
    fn default() -> Self {
        Self {
            compression: Compression::default(),
            backend: OutputBackend::Buffered,
            direct_reads: false,
            force: false,
        }
    }
}
