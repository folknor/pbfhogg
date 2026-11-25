//! Refusal assertions for the crates whose errors name their cause only in
//! text: devkit, datasets, library, harness, cli, bench and source-policy
//! report `String`s, `io::Error`s or clap errors, never a kind enum. A bare
//! `is_err()` passes on a different refusal, or on a caught panic, the same
//! as on the promised one; these name the refusal the test promises.

use std::any::Any;
use std::fmt::Display;

/// `result` is the refusal whose report contains `needle`; evaluates to the
/// error. `Ok` fails without formatting the value, and an empty needle
/// fails, so every call names a cause.
///
/// # Panics
///
/// If `result` is `Ok`, `needle` is empty, or the report lacks `needle`.
#[track_caller]
pub fn refused<T, E: Display>(result: Result<T, E>, needle: &str) -> E {
    match result {
        Ok(_) => panic!("expected a refusal containing {needle:?}, got Ok"),
        Err(error) => {
            assert!(!needle.is_empty(), "a refusal needs the text it promises");
            let text = error.to_string();
            assert!(
                text.contains(needle),
                "expected a refusal containing {needle:?}, got: {text}"
            );
            error
        }
    }
}

/// `result`, from `catch_unwind` or a thread join, is an unwind whose panic
/// message contains `needle`; evaluates to the message. A payload that is
/// not a string fails: the message is what tells the promised panic from
/// any other.
///
/// # Panics
///
/// If `result` is `Ok`, `needle` is empty, the payload is not a string, or
/// the message lacks `needle`.
#[track_caller]
pub fn unwound<T>(result: Result<T, Box<dyn Any + Send>>, needle: &str) -> String {
    let payload = match result {
        Ok(_) => panic!("expected a panic containing {needle:?}, got a return"),
        Err(payload) => payload,
    };
    assert!(!needle.is_empty(), "an unwind needs the text it promises");
    let text = if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else if let Some(text) = payload.downcast_ref::<&'static str>() {
        (*text).to_owned()
    } else {
        panic!("expected a panic containing {needle:?}, got a non-string payload")
    };
    assert!(
        text.contains(needle),
        "expected a panic containing {needle:?}, got: {text}"
    );
    text
}

#[cfg(test)]
mod tests {
    use super::{refused, unwound};

    #[test]
    fn a_refusal_must_name_its_text() {
        let wrong = std::panic::catch_unwind(|| refused(Err::<(), _>("other"), "promised"));
        unwound(
            wrong,
            "expected a refusal containing \"promised\", got: other",
        );
        let ok = std::panic::catch_unwind(|| refused(Ok::<(), &str>(()), "promised"));
        unwound(ok, "got Ok");
        let empty = std::panic::catch_unwind(|| refused(Err::<(), _>("any"), ""));
        unwound(empty, "a refusal needs the text it promises");
        assert_eq!(
            refused(Err::<(), _>("the promised one"), "promised"),
            "the promised one"
        );
    }

    #[test]
    fn an_unwind_must_name_its_text() {
        let caught = std::panic::catch_unwind(|| panic!("boom {}", 1));
        assert_eq!(unwound(caught, "boom 1"), "boom 1");
        let payload = std::panic::catch_unwind(|| std::panic::panic_any(7_u8));
        let wrong =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unwound(payload, "7")));
        unwound(wrong, "got a non-string payload");
    }
}
