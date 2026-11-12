//! The replacement matrix's two distinct reopen strategies.
//!
//! Every replacement cell injects its pin: the pinned strategy reopens
//! through a stable scratch hard link, the path strategy through verified
//! path reopening, so neither depends on procfs and no host runs one arm
//! twice. Every command under the matrix takes a caller pin. This does not
//! test procfs reopening itself; the pin's own unit tests do. Off Unix
//! there is no identity verification to assert after replacement, so the
//! cell skips before arming anything.

/// The identity behavior a replacement scenario exercises. The stable-name
/// case uses a scratch hard link, so neither case needs procfs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scenario {
    Pinned,
    Path,
}

impl Scenario {
    /// Run with an injected reopen name. The closure must pass this pin to
    /// the command, rather than passing the input path and taking a new pin.
    /// The scratch name lives until the closure and all its command work end.
    pub fn run(
        self,
        input: &std::path::Path,
        host: &pbfhogg::host::Host,
        run: impl FnOnce(&pbfhogg::read::input::PinnedInput, Self),
    ) {
        #[cfg(unix)]
        {
            let scratch = crate::common::test_scratch();
            let stable = scratch.child("pinned-input");
            let reopen = match self {
                Self::Pinned => {
                    std::fs::hard_link(input, &stable).expect("link pinned input in scratch");
                    Some(stable.as_path())
                }
                Self::Path => None,
            };
            let pin = pbfhogg::read::input::PinnedInput::pin_with_reopen_path(input, reopen, host)
                .expect("pin input with injected reopen name");
            run(&pin, self);
        }
        #[cfg(not(unix))]
        {
            let _ = (self, input, host, run);
            crate::common::skip(
                "input replacement cell: Unix file identity verification is unavailable",
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn injected_reopen_scenarios_verify_replacement_without_procfs() {
    use pbfhogg::read::input::InputFile;
    use std::io::Read as _;

    for scenario in [Scenario::Pinned, Scenario::Path] {
        let scratch = crate::common::test_scratch();
        let input = scratch.child("input");
        let replacement = scratch.child("replacement");
        std::fs::write(&input, b"original").expect("write input");
        std::fs::write(&replacement, b"replacement").expect("write replacement");
        let host = crate::common::test_host();
        scenario.run(&input, &host, |pin, expected| {
            assert!(!pin.reopens_through_procfs());
            let first = InputFile::open(pin).expect("first open");
            let second = InputFile::open(pin).expect("second open");
            let mut byte = [0];
            (&**first.file()).read_exact(&mut byte).expect("read first");
            (&**second.file())
                .read_exact(&mut byte)
                .expect("read second");
            assert_eq!(byte, *b"o", "each open has its own offset");
            std::fs::rename(&replacement, &input).expect("replace input");
            match expected {
                Scenario::Pinned => {
                    std::fs::remove_file(&input).expect("unlink replacement");
                    let reopened = InputFile::open(pin).expect("reopen original inode");
                    let mut bytes = Vec::new();
                    (&**reopened.file())
                        .read_to_end(&mut bytes)
                        .expect("read original");
                    assert_eq!(bytes, b"original");
                }
                Scenario::Path => {
                    let err = InputFile::open(pin).expect_err("refuse replaced inode");
                    assert!(matches!(err.kind(), pbfhogg::ErrorKind::InputReplaced));
                    assert_eq!(err.source_path(), Some(input.as_path()));
                }
            }
        });
    }
}

#[cfg(unix)]
#[test]
fn injected_reopen_name_must_identify_the_pinned_file() {
    let scratch = crate::common::test_scratch();
    let input = scratch.child("input");
    let other = scratch.child("other");
    std::fs::write(&input, b"same bytes").expect("write input");
    std::fs::write(&other, b"same bytes").expect("write another inode");
    let err = pbfhogg::read::input::PinnedInput::pin_with_reopen_path(
        &input,
        Some(&other),
        &crate::common::test_host(),
    )
    .expect_err("reject another inode even with identical contents");
    assert!(matches!(err.kind(), pbfhogg::ErrorKind::InputReplaced));
    assert_eq!(err.source_path(), Some(input.as_path()));
}
