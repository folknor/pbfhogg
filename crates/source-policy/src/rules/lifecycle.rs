//! Sealed lifecycle shapes: the CLI dispatch and the scratch probe lock.
//!
//! - The CLI dispatch is owned by `finish_invocation`, whose complete body
//!   runs the dispatch and then both counter hooks; `run` calls it directly,
//!   once as a statement outside every delimiter, and no other code in the
//!   CLI file names a counter hook. Library textlint rules restrict callers
//!   in other files.
//! - The complete `open_probe_lock` constructor is sealed to a noncreating,
//!   nontruncating read-write option chain, and `probe_lock` opens through it
//!   and nothing else.
//!
//! Bodies are compared token for token with comments dropped and literals
//! opaque, so layout and comments never change the verdict and a literal or
//! a commented-out copy cannot supply the shape. This checks fixed source
//! shapes, not Rust control flow: a changed shape needs an explicit audit.
//! Only `open_probe_lock` and `finish_invocation` are sealed whole; `run`
//! and `probe_lock` are held by occurrence, delimiter-depth and forbidden-
//! opener checks. A syn rewrite was weighed and refused: it would compare
//! the same fixed shapes, prove no more control flow, and stop seeing macro
//! input. What the shapes cannot show is carried by behaviour tests: the
//! probe constructor's missing-file and contents tests in scratch.rs and the
//! success, refusal and cancellation test of finish_invocation in the CLI.

use crate::Corpus;
use crate::lex::{Source, function_body};

const CLI: &str = "crates/cli/src/lib.rs";
const SCRATCH: &str = "crates/lib/src/scratch.rs";

pub const PROBE_OPEN: &str = "
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
";
pub const FINALIZE: &str = "
    let result = work();
    pbfhogg::write::emit_writer_counters(invocation);
    pbfhogg::read::emit_read_counters(invocation);
    result
";

pub fn errors(corpus: &Corpus) -> Vec<String> {
    let (Some(cli), Some(scratch)) = (corpus.files.get(CLI), corpus.files.get(SCRATCH)) else {
        return vec![format!("source lifecycle: {CLI} or {SCRATCH} missing")];
    };
    shape_errors(cli, scratch)
        .into_iter()
        .map(|message| format!("source lifecycle: {message}"))
        .collect()
}

fn compact_text(text: &str) -> String {
    let source = Source::new(text.to_owned());
    source.compact(&source.code())
}

fn sealed(source: &Source, name: &str, expected: &str) -> bool {
    function_body(source, name).is_some_and(|body| source.compact(&body) == compact_text(expected))
}

pub fn shape_errors(cli: &Source, scratch: &Source) -> Vec<&'static str> {
    let mut found = Vec::new();
    if !sealed(scratch, "open_probe_lock", PROBE_OPEN) {
        found.push(
            "scratch probe constructor must use exactly the audited noncreating option chain",
        );
    }
    let probe = function_body(scratch, "probe_lock").map(|body| scratch.compact(&body));
    // Seal the open expression at the match, not an unrelated builder or a
    // string elsewhere in the function. No second opener is admitted.
    let probe_ok = probe.is_some_and(|code| {
        code.contains("letfile=matchopen_probe_lock(&lock){")
            && !["OpenOptions", "File::open", ".open("]
                .iter()
                .any(|opener| code.contains(opener))
    });
    if !probe_ok {
        found.push("probe_lock must open its descriptor through open_probe_lock only");
    }
    if !sealed(cli, "finish_invocation", FINALIZE) {
        found.push("CLI finalization must run work and then both counter hooks unconditionally");
    }
    match function_body(cli, "run") {
        None => found.push("CLI run function missing"),
        Some(body) => found.extend(run_errors(&cli.compact(&body))),
    }
    // The CLI file is excluded wholesale from the exit-hook textlint rules,
    // so this is what keeps a hook out of every other function there: each
    // hook name is one identifier in the file's code, the one inside the
    // sealed finish_invocation. Counting names, not calls, also refuses a
    // use-alias or a function-pointer pass.
    for hook in ["emit_writer_counters", "emit_read_counters"] {
        let named = cli
            .code()
            .iter()
            .filter(|tok| cli.name(tok) == Some(hook))
            .count();
        if named != 1 {
            found.push("CLI counter hooks are named only inside finish_invocation");
        }
    }
    found
}

fn run_errors(code: &str) -> Vec<&'static str> {
    let mut found = Vec::new();
    let call =
        "letresult=finish_invocation(&host.invocation,||->Result<(),Box<dynstd::error::Error>>{";
    // The dispatch owner must be a direct statement in run, not inside an
    // unused closure, nested function, unreachable branch or macro input
    // such as stringify!, so no delimiter of any kind may be open before it.
    let direct = code.find(call).is_some_and(|start| {
        let before = &code[..start];
        [('{', '}'), ('(', ')'), ('[', ']')]
            .iter()
            .all(|&(open, close)| before.matches(open).count() == before.matches(close).count())
    });
    if !direct {
        found.push("CLI run must dispatch directly through finish_invocation");
    }
    if code.matches("finish_invocation(").count() != 1 {
        found.push("CLI run must finalize exactly one dispatch");
    }
    if code.contains("emit_writer_counters(") || code.contains("emit_read_counters(") {
        found.push("CLI run counter hooks belong in finish_invocation");
    }
    found
}

#[cfg(test)]
mod tests {
    use super::{FINALIZE, PROBE_OPEN, shape_errors};
    use crate::lex::Source;

    fn cli() -> String {
        format!(
            "
fn finish_invocation<T>(invocation: &Invocation, work: impl FnOnce() -> T) -> T {{
{FINALIZE}
}}
fn run() {{
    let result = finish_invocation(&host.invocation, || -> Result<(), Box<dyn std::error::Error>> {{
        work()
    }});
    report(result);
}}
"
        )
    }

    fn scratch() -> String {
        format!(
            "
fn open_probe_lock(path: &Path) -> io::Result<File> {{
{PROBE_OPEN}
}}
fn probe_lock(dir: &Path) -> Probe {{
    let lock = dir.join(LOCK_NAME);
    let file = match open_probe_lock(&lock) {{ Ok(file) => file, Err(e) => return unknown(e) }};
    probe(file)
}}
"
        )
    }

    fn errors(cli: &str, scratch: &str) -> Vec<&'static str> {
        shape_errors(
            &Source::new(cli.to_owned()),
            &Source::new(scratch.to_owned()),
        )
    }

    #[test]
    fn the_audited_shapes_pass() {
        assert_eq!(errors(&cli(), &scratch()), Vec::<&str>::new());
    }

    #[test]
    fn each_forbidden_probe_option_fails() {
        for option in [
            "create(true)",
            "create(false)",
            "create_new(true)",
            "truncate(true)",
            "write(false)",
            "read(false)",
            "custom_flags(libc::O_CREAT)",
        ] {
            let changed = scratch().replace(".open(path)", &format!(".{option}.open(path)"));
            assert!(!errors(&cli(), &changed).is_empty(), "{option}");
        }
    }

    #[test]
    fn probe_delegation_and_unrelated_builders_fail() {
        let other = format!("{PROBE_OPEN}; other(path)");
        for body in [
            "helper(path)",
            "let s = \".write(true)\"; helper(path)",
            &other,
        ] {
            let changed = scratch().replace(PROBE_OPEN, body);
            assert!(!errors(&cli(), &changed).is_empty(), "{body}");
        }
        let changed = scratch().replace("match open_probe_lock(&lock)", "match other(&lock)");
        assert!(!errors(&cli(), &changed).is_empty());
    }

    #[test]
    fn fake_counter_calls_fail() {
        let raw = format!("let s = r#\"{FINALIZE}\"#; work()");
        let comment = format!("/* {FINALIZE} */ work()");
        let nested = format!("fn unused() {{ {FINALIZE} }} work()");
        let dead = format!("if false {{ {FINALIZE} }} work()");
        for body in [&raw, &comment, &nested, &dead, "work()"] {
            assert!(
                !errors(&cli().replace(FINALIZE, body), &scratch()).is_empty(),
                "{body}"
            );
        }
    }

    #[test]
    fn the_dispatch_cannot_hide_in_a_branch_or_closure() {
        let start = "let result = finish_invocation";
        for (prefix, close) in [
            ("if false { ", "}"),
            ("let unused = || { ", "}"),
            ("fn unused() { ", "}"),
            ("stringify!( ", ");"),
            ("stringify![ ", "];"),
        ] {
            let changed = cli()
                .replace(start, &format!("{prefix}{start}"))
                .replace("report(result);", &format!("{close} report(result);"));
            assert!(!errors(&changed, &scratch()).is_empty(), "{prefix}");
        }
    }

    #[test]
    fn a_hook_named_outside_finish_invocation_fails() {
        for extra in [
            "fn other() { pbfhogg::write::emit_writer_counters(i); }",
            "fn other() { pbfhogg::read::emit_read_counters(i); }",
            "use pbfhogg::write::emit_writer_counters as flush;",
            "fn other() { call(pbfhogg::read::emit_read_counters); }",
        ] {
            let changed = format!("{}{extra}", cli());
            assert!(!errors(&changed, &scratch()).is_empty(), "{extra}");
        }
        let quoted = format!("{}const S: &str = \"emit_writer_counters(i)\";", cli());
        assert_eq!(errors(&quoted, &scratch()), Vec::<&str>::new());
    }

    #[test]
    fn missing_and_duplicate_helpers_fail() {
        assert!(!errors(&cli().repeat(2), &scratch()).is_empty());
        assert!(!errors(&cli(), &scratch().repeat(2)).is_empty());
        assert!(!errors("", "").is_empty());
    }

    #[test]
    fn comments_and_layout_do_not_change_a_valid_shape() {
        let cli = cli().replace("let result", "/* nested /* comment */ */ let\nresult");
        let scratch = scratch().replace(".write(true)", ".write( /* writing */ true )");
        assert_eq!(errors(&cli, &scratch), Vec::<&str>::new());
    }
}
