//! The pbfhogg library harness: the worker launched by pbfhogg-library
//! and the library-scenario bench (`reference/library-protocol.md`). It links the
//! library of the tree under test and runs scripted sequences of library
//! calls in one process.
//!
//! Three modes, and nothing else on the command line:
//!
//! - `--describe-harness` prints the describe document on stdout.
//! - `--test-harness REQUEST --private-dir DIR --artefact-dir DIR` runs the
//!   one scenario the request names and writes its documents into the
//!   artefact directory (the library driver's launch).
//! - `--bench-harness SCENARIO --work DIR [--dataset VARIANT PATH]...` lays
//!   the scenario's roles out under DIR itself and runs its workload with no
//!   judgment and no artefacts (the library-scenario bench's launch).
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
        [mode, scenario, work_flag, work, datasets @ ..]
            if mode == "--bench-harness" && work_flag == "--work" =>
        {
            match (scenario.to_str(), bench_datasets(datasets)) {
                (Some(scenario), Some(datasets)) => {
                    runner::bench_from_env(scenario, std::path::Path::new(work), &datasets)
                }
                _ => runner::Exit::Refused,
            }
        }
        _ => runner::Exit::Refused,
    };
    std::process::ExitCode::from(status.code())
}

/// `--dataset VARIANT PATH` repeated; `None` for anything else.
fn bench_datasets(args: &[std::ffi::OsString]) -> Option<Vec<(String, std::path::PathBuf)>> {
    let mut out = Vec::new();
    for triple in args.chunks(3) {
        let [flag, variant, path] = triple else {
            return None;
        };
        if flag != "--dataset" {
            return None;
        }
        out.push((variant.to_str()?.to_owned(), std::path::PathBuf::from(path)));
    }
    Some(out)
}
