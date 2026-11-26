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

use crate::pinned_cell::{self, Cell, Judged};

use std::path::Path;

use pbfhogg::digest::{Digest, Form};
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
) -> pbfhogg::Result<Digest> {
    pbfhogg::digest::digest(input, Form::Full, direct, host)
}

/// The unreplaced input's digest, under a host of its own.
fn expected(input: &Path) -> Digest {
    run(input, false, &crate::common::test_host()).expect("the unreplaced input")
}

/// One open, through the caller's pin, in every read mode through
/// [`pinned_cell::check`]: armed past it, the replacement never fires and
/// only counts, and the open is direct on a direct run. Replaced right
/// after it, the run reads only the file it opened, whichever way a later
/// open would have reached the pin: nothing opens again.
#[test]
fn digest_opens_its_input_once() {
    for &direct in pinned_cell::modes() {
        let dir = crate::common::test_scratch();
        let input = dir.path().join("in.osm.pbf");
        let replacement = dir.path().join("replacement.osm.pbf");
        write_input(&input);
        let expected = expected(&input);
        let cell = Cell::new("digest", direct, &input, &replacement, 1, "D");
        pinned_cell::check(
            &cell,
            |host| run(&PinnedInput::pin(&input, host)?, direct, host),
            |digest| {
                assert_eq!(digest, expected, "{}", cell.what());
                digest
            },
            |_, _| {
                write_input(&input);
                write_replacement(&replacement);
            },
            |(), step| run(step.pin, direct, step.host),
            |expected, (), step, judged| {
                // Only the caller-pin arm (replaced before the call) can
                // refuse: after its one open nothing opens again.
                let Judged::Unaffected(digest) = judged else {
                    assert_eq!(step.after, 0, "{}: the one open is the last", step.what);
                    return;
                };
                assert_eq!(
                    &digest, expected,
                    "{}: the run must read only the file it opened",
                    step.what
                );
            },
        );
    }
}
