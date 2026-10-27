//! digest against an input renamed over mid-run, through the fault plan on
//! the run's token (`arm_input_replacement`, see `pbfhogg::test_hooks`).
//!
//! digest opens its input once and reads the envelope, the indexdata and
//! every block through that open, so it takes no pin of its own: with a path
//! there is no later open for a replacement to reach. Opens are counted only
//! through a pin, so these tests hand digest a caller's pin, which it opens
//! through as it is: once, and a replacement after that open never reaches
//! the run, in either reopen mode. These count only opens through the pin;
//! that no other open of the input happens (a path open beside the pin's)
//! is shown by `digest_input.rs`'s FIFO test, which a second open of the
//! FIFO would hang or starve.
//!
//! The replacement carries other IDs, so a digest of it differs.

use std::path::Path;
use std::sync::atomic::Ordering;

use pbfhogg::digest::ContentDigest;
use pbfhogg::read::input::{InputSource, PinnedInput};

use crate::common::{TestNode, write_multi_block_test_pbf};

/// Sorted nodes `shift + 1 ..= shift + 4`, one per data blob.
fn write_fixture(path: &Path, shift: i64) {
    let nodes: Vec<TestNode> = (1..=4)
        .map(|id| TestNode {
            id: id + shift,
            lat: 10,
            lon: 20,
            tags: vec![("name", "x")],
            meta: None,
        })
        .collect();
    write_multi_block_test_pbf(path, &nodes, &[], &[], 1);
}

fn write_input(path: &Path) {
    write_fixture(path, 0);
}

fn write_replacement(path: &Path) {
    write_fixture(path, 500);
}

fn run<'a>(
    input: impl Into<InputSource<'a>>,
    direct: bool,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<ContentDigest> {
    pbfhogg::digest::digest(input, direct, host)
}

/// The unreplaced input's digest, under a host of its own.
fn expected(input: &Path) -> ContentDigest {
    run(input, false, &crate::common::test_host()).expect("the unreplaced input")
}

/// One open, through the caller's pin, buffered or direct: armed past it,
/// the replacement never fires and only counts.
#[test]
fn digest_opens_its_input_once() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    write_input(&input);
    let expected = expected(&input);
    let directs: &[bool] = if cfg!(feature = "linux-direct-io") {
        &[false, true]
    } else {
        &[false]
    };
    for &direct in directs {
        let host = crate::common::test_host();
        let pin = PinnedInput::pin(&input, &host).expect("caller pin");
        host.invocation.faults().arm_input_replacement(
            &input,
            &dir.path().join("unused.osm.pbf"),
            2,
        );
        let digest = match run(&pin, direct, &host) {
            Ok(digest) => digest,
            // Only a typed O_DIRECT setup refusal skips; see `is_einval`.
            #[cfg(feature = "linux-direct-io")]
            Err(e) if direct && crate::common::is_einval(&e) => {
                crate::common::skip(format!("direct {direct}: {e}"));
                continue;
            }
            Err(e) => panic!("direct {direct}: {e}"),
        };
        assert_eq!(digest, expected, "direct {direct}");
        let report = host
            .invocation
            .faults()
            .input_replacement()
            .expect("armed replacement");
        assert!(!report.fired, "direct {direct}: {report:?}");
        assert_eq!(report.attempts, 1, "direct {direct}: {report:?}");
        assert_eq!(report.verified, 1, "direct {direct}: {report:?}");
        assert_eq!(report.refused, 0, "direct {direct}: {report:?}");
    }
}

/// Replaced right after the one open, the run reads only the file it
/// opened, whichever way a later open would have reached the pin: nothing
/// opens again.
#[test]
fn a_replacement_after_the_open_does_not_reach_the_run() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    let replacement = dir.path().join("replacement.osm.pbf");
    for force_path in [true, false] {
        write_input(&input);
        let expected = expected(&input);
        write_replacement(&replacement);
        let host = crate::common::test_host();
        host.invocation
            .faults()
            .force_pin_path_reopen
            .store(force_path, Ordering::Relaxed);
        let pin = PinnedInput::pin(&input, &host).expect("caller pin");
        let procfs = pin.reopens_through_procfs();
        host.invocation
            .faults()
            .arm_input_replacement(&input, &replacement, 1);
        let result = run(&pin, false, &host);
        let cell = format!("procfs {procfs}");
        let report = host
            .invocation
            .faults()
            .input_replacement()
            .expect("armed replacement");
        assert!(
            report.fired,
            "{cell}: the replacement must fire: {report:?}"
        );
        assert_eq!(
            result.unwrap_or_else(|e| panic!("{cell}: {e}")),
            expected,
            "{cell}: the run must read only the file it opened"
        );
        assert_eq!(report.attempts, 1, "{cell}: {report:?}");
        assert_eq!(report.attempts_after_fired, 0, "{cell}: {report:?}");
    }
}

/// A caller's pin is the run's: replaced between the caller's pin and the
/// call, the input is the file the caller pinned (procfs) or the one open is
/// refused (path reopen). Opening the path afresh would digest the
/// replacement.
#[test]
fn a_caller_pin_is_used_as_it_is() {
    let dir = crate::common::test_scratch();
    let input = dir.path().join("in.osm.pbf");
    let replacement = dir.path().join("replacement.osm.pbf");
    for force_path in [true, false] {
        write_input(&input);
        let expected = expected(&input);
        write_replacement(&replacement);
        let host = crate::common::test_host();
        host.invocation
            .faults()
            .force_pin_path_reopen
            .store(force_path, Ordering::Relaxed);
        let pin = PinnedInput::pin(&input, &host).expect("caller pin");
        let procfs = pin.reopens_through_procfs();
        std::fs::rename(&replacement, &input).expect("replace the input");
        let result = run(&pin, false, &host);
        let cell = format!("procfs {procfs}");
        if procfs {
            assert_eq!(
                result.unwrap_or_else(|e| panic!("{cell}: {e}")),
                expected,
                "{cell}: the digest must be the pinned file's"
            );
        } else if cfg!(unix) {
            // Off Unix a pin verifies nothing; the identity refusal is a
            // Unix guarantee.
            let err = result
                .err()
                .unwrap_or_else(|| panic!("{cell}: must refuse"));
            assert!(
                matches!(err.kind(), pbfhogg::ErrorKind::InputReplaced),
                "{cell}: {err:?}"
            );
        }
    }
}
