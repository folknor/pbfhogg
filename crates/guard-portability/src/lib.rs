//! Compile fixture for the promise guard (`crates/cli/tests/guard.rs`) on an
//! architecture it does not support. The guard compiles its seccomp filter
//! only on little-endian x86_64 and aarch64 and falls back to running children
//! unguarded everywhere else; this crate is what type-checks that fallback.
//!
//! The mount is test-only and limited to the unsupported architectures: the
//! guard is test code, and its supported half needs the CLI test crate's
//! `common` module, which this crate does not have. On the host this crate is
//! therefore empty. The `riscv64` lint sweep in brokkr.d/checks.toml checks it
//! with `--all-targets`; that sweep must name a triple outside the guard's
//! supported set, or it checks nothing.

#[cfg(all(
    test,
    target_os = "linux",
    not(all(
        target_endian = "little",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))
))]
#[path = "../../cli/tests/guard.rs"]
pub mod guard;
