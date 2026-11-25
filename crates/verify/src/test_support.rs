//! Assertions the verifier's unit tests share.

/// `result` is the refusal whose report contains `needle`; evaluates to the
/// error. The verifier's parsers and comparators report `String` (or the
/// crate `Error`, whose text is its detail), so the message is the only
/// thing that names which refusal a test promises: a bare `is_err()` would
/// pass on a different refusal, or on a caught panic, the same as on the
/// promised one. `Ok` fails without formatting the value.
#[track_caller]
pub(crate) fn refused<T, E: std::fmt::Display>(result: Result<T, E>, needle: &str) -> E {
    match result {
        Ok(_) => panic!("expected a refusal containing {needle:?}, got Ok"),
        Err(error) => {
            let text = error.to_string();
            assert!(!needle.is_empty(), "a refusal needs the text it promises");
            assert!(
                text.contains(needle),
                "expected a refusal containing {needle:?}, got: {text}"
            );
            error
        }
    }
}
