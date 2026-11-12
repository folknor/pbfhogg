//! Every timed wait says why its expiry is not an inference.
//!
//! A test never infers that another thread blocked from elapsed time: it
//! waits, bounded, for an observation made at the wait itself
//! (`reference/testing.md`, "Test time bounds"). The shapes that break that
//! rule are a sleep before an assertion and a timed receive or condvar wait
//! whose expiry is taken as the answer. This rule makes each one visible to
//! review: every code identifier in [`TIMED`], anywhere in the corpus,
//! production included, needs a `// wait-deliberate: <reason>` line comment
//! on its line or alone on the line above it. Matching identifiers rather
//! than calls catches an aliased import (`use std::thread::sleep as nap`), a
//! function reference and a UFCS path at the name itself.
//!
//! One shape passes unmarked: `recv_timeout` or `recv_deadline` called as a
//! method whose balanced call is followed directly by `.unwrap()` or
//! `.expect(`. Expiry panics at that receive, so the timeout is a failure
//! bound. That says nothing about whether the received message establishes
//! what the test then asserts, and a caller that catches the panic is
//! outside it.
//!
//! A sleep is never a deadline bounding a wait for an event, so the sleep
//! family ([`SLEEPS`], and libc's sleeping calls) is refused outright: no
//! marker exempts it, and no file is excepted. A wait on something our code
//! controls ends on a notification; a wait on something that cannot notify
//! states why through a marked bounded wait. The spin primitives
//! ([`SPINS`], named as `thread::yield_now` and `hint::spin_loop`) are
//! refused outright too: a loop around them is a poll. An executor's own
//! `yield_now` method (rayon's cooperative wait) is not that path and is not
//! matched.
//!
//! libc's raw waits ([`RAW`], named as `libc::name`) take the marker rule:
//! an explicit inventory of the spellings this tree uses, not every timed
//! syscall there is.
//!
//! A spelling tripwire, not enforcement of the prose rule: a wrapper
//! function, macro expansion and an imported `yield_now` called bare are
//! outside it, and a marker's reason is checked only for being present.
//! Spins on `Instant`, `try_recv` or an atomic load call none of these
//! names; `loop_waits` makes each such loop state what ends it.

use std::collections::BTreeSet;

use crate::Corpus;
use crate::lex::{Kind, Source, Tok};

/// std's sleeps: refused everywhere.
pub const SLEEPS: &[&str] = &["sleep", "sleep_until", "sleep_ms"];

/// libc's sleeping calls, named as `libc::name`: refused like [`SLEEPS`].
const RAW_SLEEPS: &[&str] = &["nanosleep", "clock_nanosleep", "usleep"];

/// std's spin primitives, as (module, name), matched as `module::name`:
/// refused like [`SLEEPS`].
pub const SPINS: &[(&str, &str)] = &[("thread", "yield_now"), ("hint", "spin_loop")];

/// libc's timed waits, named as `libc::name`: marker rule.
pub const RAW: &[&str] = &["poll", "ppoll", "epoll_wait", "select", "pselect"];

/// The names of std's timed waits other than sleeps: marker rule.
pub const TIMED: &[&str] = &[
    "park_timeout",
    "park_timeout_ms",
    "recv_timeout",
    "recv_deadline",
    "wait_timeout",
    "wait_timeout_ms",
    "wait_timeout_while",
];

/// The receives whose `.unwrap()` or `.expect(` makes expiry a panic.
const RECEIVES: &[&str] = &["recv_timeout", "recv_deadline"];

const MARKER: &str = "wait-deliberate:";

pub fn errors(corpus: &Corpus) -> Vec<String> {
    let mut found: Vec<String> = super::require_files(corpus, "crates/", "timed waits")
        .into_iter()
        .collect();
    for (path, source) in corpus.under("crates/") {
        for (line, name) in unmarked(source) {
            found.push(format!(
                "{path}:{line}: `{name}` is a timed wait; wait for an observation made at \
                 the wait itself, or say why with `// wait-deliberate: <reason>`"
            ));
        }
        for (line, name) in sleeps(source) {
            found.push(format!(
                "{path}:{line}: `{name}` sleeps; a time value survives only as a deadline \
                 bounding a wait that ends on an event: wait for the event"
            ));
        }
        for (line, name) in spins(source) {
            found.push(format!(
                "{path}:{line}: `{name}` spins; a loop around it polls: wait for a \
                 notification (a bell, a condition variable, a channel) instead"
            ));
        }
    }
    found
}

/// `module :: name` at `i`.
fn is_path(source: &Source, code: &[Tok], i: usize, module: &str) -> bool {
    i >= 3
        && source.is_punct(&code[i - 1], ":")
        && source.is_punct(&code[i - 2], ":")
        && source.name(&code[i - 3]) == Some(module)
}

/// `libc :: name` at `i`.
fn is_libc(source: &Source, code: &[Tok], i: usize) -> bool {
    is_path(source, code, i, "libc")
}

/// Every spin primitive named by its module path, with its line.
pub fn spins(source: &Source) -> Vec<(usize, String)> {
    let code = source.code();
    let mut out = Vec::new();
    for (i, tok) in code.iter().enumerate() {
        let Some(name) = source.name(tok) else {
            continue;
        };
        if let Some((module, _)) = SPINS
            .iter()
            .find(|(module, spin)| *spin == name && is_path(source, &code, i, module))
        {
            out.push((tok.line, format!("{module}::{name}")));
        }
    }
    out
}

/// Every sleep identifier, marked or not, with its line.
pub fn sleeps(source: &Source) -> Vec<(usize, String)> {
    let code = source.code();
    let mut out = Vec::new();
    for (i, tok) in code.iter().enumerate() {
        let Some(name) = source.name(tok) else {
            continue;
        };
        if SLEEPS.contains(&name) || (RAW_SLEEPS.contains(&name) && is_libc(source, &code, i)) {
            out.push((tok.line, name.to_owned()));
        }
    }
    out
}

/// Every timed-wait identifier no marker covers, with its line. Sleeps are
/// [`sleeps`]' business, marked or not.
pub fn unmarked(source: &Source) -> Vec<(usize, String)> {
    let code = source.code();
    let marked = marked_lines(source);
    let mut out = Vec::new();
    for (i, tok) in code.iter().enumerate() {
        let Some(name) = source.name(tok) else {
            continue;
        };
        let timed = TIMED.contains(&name) || (RAW.contains(&name) && is_libc(source, &code, i));
        if !timed || marked.contains(&tok.line) {
            continue;
        }
        if RECEIVES.contains(&name) && panics_on_expiry(source, &code, i) {
            continue;
        }
        out.push((tok.line, name.to_owned()));
    }
    out
}

/// `. name ( ... ) . unwrap ( )` or `. name ( ... ) . expect (` at `i`.
fn panics_on_expiry(source: &Source, code: &[Tok], i: usize) -> bool {
    let is = |j: usize, p: &str| code.get(j).is_some_and(|t| source.is_punct(t, p));
    if i == 0 || !is(i - 1, ".") || !is(i + 1, "(") {
        return false;
    }
    let Some(close) = matching_paren(source, code, i + 1) else {
        return false;
    };
    if !is(close + 1, ".") || !is(close + 3, "(") {
        return false;
    }
    let Some(method) = code.get(close + 2).and_then(|t| source.name(t)) else {
        return false;
    };
    match method {
        "unwrap" => is(close + 4, ")"),
        "expect" => true,
        _ => false,
    }
}

fn matching_paren(source: &Source, code: &[Tok], open: usize) -> Option<usize> {
    let mut depth = 0_usize;
    for (j, tok) in code.iter().enumerate().skip(open) {
        if source.is_punct(tok, "(") {
            depth += 1;
        } else if source.is_punct(tok, ")") {
            depth = depth.checked_sub(1)?;
            if depth == 0 {
                return Some(j);
            }
        }
    }
    None
}

fn is_marker(comment: &str) -> bool {
    comment
        .strip_prefix("//")
        .map(str::trim_start)
        .and_then(|rest| rest.strip_prefix(MARKER))
        .is_some_and(|reason| !reason.trim_start().is_empty())
}

/// Lines a marker covers: its own, and the next when it stands alone.
fn marked_lines(source: &Source) -> BTreeSet<usize> {
    let mut code_lines = BTreeSet::new();
    for tok in source.code() {
        code_lines.extend(tok.line..=tok.end_line);
    }
    let mut marked = BTreeSet::new();
    for tok in &source.toks {
        if tok.kind == Kind::LineComment && is_marker(source.text_of(tok)) {
            marked.insert(tok.line);
            if !code_lines.contains(&tok.line) {
                marked.insert(tok.line + 1);
            }
        }
    }
    marked
}

#[cfg(test)]
mod tests {
    use super::{sleeps, spins, unmarked};
    use crate::lex::Source;

    fn names(src: &str) -> Vec<String> {
        unmarked(&Source::new(src.to_owned()))
            .into_iter()
            .map(|(_, name)| name)
            .collect()
    }

    fn sleep_names(src: &str) -> Vec<String> {
        sleeps(&Source::new(src.to_owned()))
            .into_iter()
            .map(|(_, name)| name)
            .collect()
    }

    #[test]
    fn sleeps_are_found_marked_or_not() {
        for src in [
            "fn f() { std::thread::sleep(d); }",
            "use std::thread::sleep as nap;",
            "fn f() { let p = std::thread::sleep; }",
            "fn f() { r#sleep(d); }",
            "fn f() { std::thread::sleep(d); } // wait-deliberate: a reason",
            "fn f() { libc::nanosleep(a, b); }",
            "fn f() { libc::usleep(1); }",
        ] {
            assert_eq!(sleep_names(src).len(), 1, "{src}");
            assert!(
                names(src).is_empty(),
                "{src}: sleeps are not marker business"
            );
        }
        for src in [
            "// std::thread::sleep(d);",
            "fn f() { let s = \"std::thread::sleep(d)\"; }",
            "fn f() { asleep(d); sleeper(); }",
            "fn f() { my::nanosleep(a); }",
        ] {
            assert!(sleep_names(src).is_empty(), "{src}");
        }
    }

    #[test]
    fn spins_are_found_by_their_path() {
        let spin_names = |src: &str| -> Vec<String> {
            spins(&Source::new(src.to_owned()))
                .into_iter()
                .map(|(_, name)| name)
                .collect()
        };
        for (src, name) in [
            ("fn f() { std::thread::yield_now(); }", "thread::yield_now"),
            ("use std::thread::yield_now;", "thread::yield_now"),
            ("fn f() { std::hint::spin_loop(); }", "hint::spin_loop"),
            (
                "fn f() { std::thread::yield_now(); } // wait-deliberate: a reason",
                "thread::yield_now",
            ),
        ] {
            assert_eq!(spin_names(src), [name], "{src}");
        }
        for src in [
            "fn f() { executor.yield_now(); }",
            "fn f() { rayon::yield_now(); }",
            "// std::thread::yield_now();",
            "fn f() { let s = \"std::hint::spin_loop()\"; }",
        ] {
            assert!(spin_names(src).is_empty(), "{src}");
        }
    }

    #[test]
    fn libc_raw_waits_take_the_marker_rule() {
        assert_eq!(names("fn f() { libc::poll(p, 1, t); }"), ["poll"]);
        assert_eq!(names("fn f() { libc::ppoll(p, 1, t, s); }"), ["ppoll"]);
        for src in [
            "fn f() { libc::poll(p, 1, t); } // wait-deliberate: a reason",
            "fn f() { fut.poll(cx); }",
            "fn f() { Pin::new(&mut f).poll(cx); }",
        ] {
            assert!(names(src).is_empty(), "{src}");
        }
    }

    #[test]
    fn timed_waits_are_refused_in_every_spelling() {
        for src in [
            "use std::thread::park_timeout as nap;",
            "fn f() { let p = std::thread::park_timeout; }",
            "fn f() { r#park_timeout(d); }",
            "fn f() { std::thread::park_timeout(d); }",
            "fn f() { Receiver::recv_timeout(&rx, d).unwrap(); }",
            "fn f() { let _ = rx.recv_timeout(d); }",
            "fn f() { match rx.recv_timeout(d) { _ => {} } }",
            "fn f() { rx.recv_timeout(d).unwrap_or(0); }",
            "fn f() { rx.recv_timeout(d).unwrap(x); }",
            "fn f() { cv.wait_timeout(g, d).unwrap(); }",
            "fn f() { cv.wait_timeout_while(g, d, |s| *s).expect(\"lock\"); }",
            "fn f() { let recv_timeout = d; }",
            "fn f() { std::thread::park_timeout(d); } // wait-deliberate:",
        ] {
            assert_eq!(names(src).len(), 1, "{src}");
        }
    }

    #[test]
    fn panicking_receives_and_marked_waits_pass() {
        for src in [
            "fn f() { rx.recv_timeout(d).unwrap(); }",
            "fn f() { rx.recv_timeout(g(h(d))).expect(\"msg\"); }",
            "fn f() {\n    rx\n        .recv_timeout(d)\n        .expect(\"msg\");\n}",
            "fn f() { rx.recv_deadline(t).unwrap(); }",
            "fn f() { std::thread::park_timeout(d); } // wait-deliberate: a reason",
            "fn f() {\n    // wait-deliberate: a reason\n    std::thread::park_timeout(d);\n}",
            "// std::thread::park_timeout(d);",
            "fn f() { let s = \"std::thread::park_timeout(d)\"; }",
            "fn f() { park_timeouts(d); }",
        ] {
            assert!(names(src).is_empty(), "{src}");
        }
    }

    #[test]
    fn a_marker_covers_only_its_line_or_the_next() {
        for src in [
            "fn f() {\n    // wait-deliberate: a reason\n\n    std::thread::park_timeout(d);\n}",
            "fn f() { let a = 1; // wait-deliberate: a reason\n    std::thread::park_timeout(d);\n}",
            "fn f() { /* // wait-deliberate: a reason */ std::thread::park_timeout(d); }",
        ] {
            assert_eq!(names(src).len(), 1, "{src}");
        }
    }
}
