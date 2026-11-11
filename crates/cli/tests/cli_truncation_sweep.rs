//! Representative CLI read-path matrix, not an inventory of every command.
//! Each named case runs one child under its own watchdog. Exhaustive framing
//! boundaries belong to read/frame_conformance_tests.rs and tests/read_paths.rs;
//! these cases pin CLI routing, error propagation, and valid-input controls.

use crate::common::adversarial::locate_blobs;
use crate::common::{
    generate_nodes, generate_relations, generate_ways, write_multi_block_test_pbf,
};
use crate::invoker::CliInvoker;

#[derive(Clone, Copy)]
enum Cut {
    Valid,
    PartialPrefix,
    CommittedPrefix,
    Header,
    Payload,
}

fn run_case(subcmd: &str, extra: &[&str], cut: Cut) {
    let dir = crate::common::test_scratch();
    let input = dir.child("input.osm.pbf");
    let nodes = generate_nodes(40, 1);
    let ways = generate_ways(10, 1_000, 3, 1);
    let relations = generate_relations(4, 10_000, 2, 1_000);
    write_multi_block_test_pbf(&input, &nodes, &ways, &relations, 10);
    let pbf = std::fs::read(&input).expect("read fixture");
    let blobs = locate_blobs(&pbf);
    // Cut the final data frame so every route first sees a complete header
    // and data. Missing-header policy cannot mask the framing diagnosis.
    let frame = blobs.last().expect("fixture has frames");
    let (len, class) = match cut {
        Cut::Valid => (pbf.len(), Classification::Valid),
        Cut::PartialPrefix => (frame.frame_start + 2, Classification::Tolerated),
        Cut::CommittedPrefix => (frame.header_start, Classification::HardError),
        Cut::Header => (frame.header_end - 1, Classification::HardError),
        Cut::Payload => (frame.blob_end - 1, Classification::HardError),
    };
    std::fs::write(&input, &pbf[..len]).expect("write case input");
    run_and_assert(&dir, subcmd, &input, len, class, extra);
}

// One declaration generates exactly five independently scheduled one-child
// tests. Adding a route cannot enlarge an existing watchdog unit. Each route
// has a valid-input control and all four CLI propagation obligations.
macro_rules! route {
    ($name:ident, $command:literal, [$($extra:literal),*], $obligation:literal) => {
        #[doc = $obligation]
        mod $name {
            use super::*;
            #[test]
            fn valid_control() {
                run_case($command, &[$($extra),*], Cut::Valid);
            }
            #[test]
            fn partial_prefix() {
                run_case($command, &[$($extra),*], Cut::PartialPrefix);
            }
            #[test]
            fn committed_prefix() {
                run_case($command, &[$($extra),*], Cut::CommittedPrefix);
            }
            #[test]
            fn truncated_header() {
                run_case($command, &[$($extra),*], Cut::Header);
            }
            #[test]
            fn truncated_payload() {
                run_case($command, &[$($extra),*], Cut::Payload);
            }
        }
    };
}

// The inventory and generated cases share one declaration. Every route must
// remain a real command in the production clap tree; new commands need not
// duplicate these representative obligations.
macro_rules! matrix {
    ($(($name:ident, $command:literal, [$($extra:literal),*], $obligation:literal)),* $(,)?) => {
        $(route!($name, $command, [$($extra),*], $obligation);)*

        #[test]
        fn representative_route_inventory() {
            let commands = crate::invoker::subcommands();
            let mut seen = std::collections::BTreeSet::new();
            $(
                assert!(commands.iter().any(|command| command == $command));
                assert!(seen.insert($command), "duplicate representative route");
            )*
        }
    };
}

matrix!(
    (
        cat,
        "cat",
        [],
        "Raw passthrough framing and CLI error propagation."
    ),
    (
        inspect,
        "inspect",
        [],
        "Read-only inspection without an output writer."
    ),
    (sort, "sort", [], "Sort scan and partial-input policy."),
    (
        getid,
        "getid",
        ["--", "n1"],
        "Selective header walk with positional blob reads."
    ),
    (
        altw,
        "add-locations-to-ways",
        [],
        "Location front probe and payload skipping."
    ),
    (
        renumber,
        "renumber",
        [],
        "Full-read renumber scan and rewrite routing."
    ),
);

/// Classification of a truncation offset.
///
/// Shape 1 is tolerated by the READER (`reference/truncation-handling.md`):
/// `BlobReader::next` and `HeaderWalker::next_header` return `Ok(None)`
/// for partial length prefixes. What commands DO with a partially-
/// populated file is per-command policy: `cat`/`inspect` may succeed
/// emitting whatever blobs they read; `sort` may legitimately reject
/// "less data than expected" because it can't sort what it doesn't
/// have. So this sweep can only pin shape 1 as no-panic + bounded
/// stderr at the command level. The reader-level tolerance contract
/// is pinned separately by integration tests on `BlobReader::next`
/// (see `crates/lib/tests/read_paths.rs::trailing_partial_length_prefix_*`).
#[derive(Debug, Clone, Copy)]
enum Classification {
    Valid,
    /// Shape 1: file ends with 0-3 leftover bytes of an incomplete
    /// next-frame length prefix.
    Tolerated,
    /// Shapes 2-4: committed length prefix or further. Reader must
    /// hard-error; command must exit non-zero.
    HardError,
}

fn run_and_assert(
    dir: &pbfhogg_test_scratch::TestScratch,
    subcmd: &str,
    input: &std::path::Path,
    len: usize,
    class: Classification,
    extra_args: &[&str],
) {
    let invocation_scratch = pbfhogg_test_scratch::scratch();
    let cell = dir.cell();
    let output = cell.join("output.osm.pbf");
    let mut inv = CliInvoker::new(&invocation_scratch).arg(subcmd).arg(input);
    if subcmd != "inspect" {
        inv = inv.arg("-o").arg(&output);
    }
    for arg in extra_args {
        inv = inv.arg(arg);
    }
    let out = inv.run();
    let stderr = out.stderr_str();
    assert!(
        !stderr.contains("panicked at"),
        "{subcmd} panicked at truncation len={len}; stderr:\n{stderr}",
    );
    assert!(
        stderr.len() < 100_000,
        "{subcmd} produced suspiciously large stderr ({}) at truncation len={len}",
        stderr.len(),
    );
    assert!(
        matches!(out.status.code(), Some(0 | 2)),
        "{subcmd} had a usage error or abnormal exit at len={len}: {:?}; {stderr}",
        out.status,
    );
    match class {
        Classification::Valid => assert!(
            out.status.success(),
            "{subcmd} valid-input control failed: {stderr}"
        ),
        Classification::Tolerated => {
            // Shape 1: reader-level tolerance is pinned by unit tests
            // (`crates/lib/tests/read_paths.rs::trailing_partial_length_prefix_*`).
            // Command-level outcome here is per-policy and not part of
            // the contract this sweep covers - sort may reject a
            // truncated tail that cuts off most data blobs even when
            // the reader correctly returns Ok(None) at the partial
            // length prefix. No-panic + bounded stderr only.
        }
        Classification::HardError => {
            assert!(
                out.status.code() == Some(2)
                    && stderr.contains("frame truncated:")
                    && stderr.contains(&input.display().to_string()),
                "{subcmd} must hard-error on a shape-2/3/4 truncation \
                 (len={len}); per `reference/truncation-handling.md` \
                 only shape 1 (0-3 bytes past a frame boundary) is \
                 tolerated. stdout:\n{}\nstderr:\n{stderr}",
                out.stdout_str(),
            );
        }
    }
}
