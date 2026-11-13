//! The pbfhogg library harness: the worker launched by pbfhogg-library
//! and the library-scenario bench (`reference/library-protocol.md`). It links the
//! library of the tree under test and runs scripted sequences of library
//! calls in one process.
//!
//! Two modes, and nothing else on the command line:
//!
//! - `--describe-harness` prints the describe document on stdout.
//! - `--test-harness REQUEST --private-dir DIR --artefact-dir DIR` runs the
//!   one scenario the request names and writes its documents into the
//!   artefact directory. The artefact directory is an argument, not an
//!   environment variable, so a script bench (which may not set `BROKKR_*`
//!   names in a child's environment) can launch the harness directly.
//!
//! stdout and stderr stay empty on every other path: a refusal is an exit
//! status (2) and, where the artefact directory is known, `refusal.json`.
//! A panic still reaches stderr, which fails the run, as it should.

mod describe;
mod describe_out;
mod diag;
mod json;
mod outcome;
mod process;
mod protocol;
mod request;
mod runner;
mod scenarios;
#[cfg(test)]
mod test_scratch;
mod workload;

// hotpath 0.16 and later count allocations only through this allocator,
// which the executable declares itself.
#[cfg(feature = "hotpath-alloc")]
#[global_allocator]
static ALLOC: hotpath::CountingAllocator = hotpath::CountingAllocator::new();

fn main() -> std::process::ExitCode {
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let status = match args.as_slice() {
        [mode] if mode == "--describe-harness" => describe::run(),
        [mode, request, flag, private_dir, artefact_flag, artefacts]
            if mode == "--test-harness"
                && flag == "--private-dir"
                && artefact_flag == "--artefact-dir" =>
        {
            runner::run_from_env(
                std::path::Path::new(request),
                std::path::Path::new(private_dir),
                std::path::Path::new(artefacts),
            )
        }
        _ => runner::Exit::Refused,
    };
    std::process::ExitCode::from(status.code())
}
