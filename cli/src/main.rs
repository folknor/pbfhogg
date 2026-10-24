//! The `pbfhogg` binary. The program is the package's library target
//! (`cli/src/lib.rs`), so its tests can reach the parsed command tree.

// hotpath 0.15 installed CountingAllocator internally under hotpath-alloc; as
// of 0.16 it is a plain generic type the consumer must declare - the crate no
// longer wires it up on its own. Without this, hotpath-alloc builds fall back
// to the system allocator and track_alloc/track_dealloc never fire, so every
// function silently reports 0 bytes. It lives in the executable alone: the
// library target is also linked into this package's test harnesses.
#[cfg(feature = "hotpath-alloc")]
#[global_allocator]
static ALLOC: hotpath::CountingAllocator = hotpath::CountingAllocator::new();

fn main() -> std::process::ExitCode {
    pbfhogg_cli::run()
}
