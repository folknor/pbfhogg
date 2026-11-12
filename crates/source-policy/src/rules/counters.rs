//! No `as i64` cast inside the arguments of a `.counter(...)` call.
//!
//! `Invocation::counter` takes unsigned integers directly and saturates them
//! at `i64::MAX`, where a cast wraps; rustfmt puts long arguments on their
//! own line, so the call has to be read whole. A user-supplied u64 whose bit
//! pattern is the meaning (degrade's drop seed) uses `cast_signed()`. The
//! scan sees the call's arguments only, including macro input there, not
//! casts made upstream of a local.

use crate::Corpus;
use crate::lex::{Kind, Source};

const TREES: [&str; 2] = ["crates/lib/src/", "crates/cli/src/"];

pub fn errors(corpus: &Corpus) -> Vec<String> {
    let mut found = Vec::new();
    for tree in TREES {
        found.extend(super::require_files(corpus, tree, "counter casts"));
        for (path, source) in corpus.under(tree) {
            for (line, args) in casts(source) {
                found.push(format!(
                    "{path}:{line}: counter argument casts `as i64` ({args}); pass the \
                     unsigned value, Invocation::counter saturates it"
                ));
            }
        }
    }
    found
}

/// (line, argument text) for every `.counter(` call casting `as i64`.
pub fn casts(source: &Source) -> Vec<(usize, String)> {
    let code = source.code();
    let mut found = Vec::new();
    for i in 0..code.len().saturating_sub(2) {
        if !(source.is_punct(&code[i], ".")
            && source.is_word(&code[i + 1], "counter")
            && source.is_punct(&code[i + 2], "("))
        {
            continue;
        }
        let mut depth = 1_usize;
        let mut j = i + 3;
        while j < code.len() && depth > 0 {
            if source.is_punct(&code[j], "(") {
                depth += 1;
            } else if source.is_punct(&code[j], ")") {
                depth -= 1;
            }
            j += 1;
        }
        let args = &code[i + 3..j.saturating_sub(1).max(i + 3)];
        let cast = args.windows(2).any(|pair| {
            source.is_word(&pair[0], "as")
                && matches!(pair[1].kind, Kind::Ident | Kind::RawIdent)
                && source.name(&pair[1]) == Some("i64")
        });
        if cast {
            let text = args
                .iter()
                .map(|t| source.text_of(t))
                .collect::<Vec<_>>()
                .join(" ");
            found.push((code[i].line, text));
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::casts;
    use crate::lex::Source;

    #[test]
    fn counter_casts_are_found_in_whole_calls_only() {
        let cases = [
            ("inv.counter(\"a\", n as i64);", 1),
            ("inv.counter(\n    \"a\",\n    n as i64,\n);", 1),
            ("inv.counter (\"a\", (n) as\n i64);", 1),
            ("inv.counter(\"a\", n as r#i64);", 1),
            ("inv.counter(\"a\", n);", 0),
            ("inv.counter(\"a\", n); // inv.counter(\"b\", m as i64)", 0),
            (
                "/* inv.counter(\"b\", m as i64) /* nested */ still */ inv.counter(\"a\", n);",
                0,
            ),
            ("inv.counter(\"a (\", n); let x = m as i64;", 0),
            ("inv.counter(r#\"a ) \"# , n); let x = m as i64;", 0),
            ("inv.counter('(', n); let x = m as i64;", 0),
            ("fn f<'a>(x: &'a str) { inv.counter(x, n as i64); }", 1),
            ("inv.counter(\"a\", count!(as \"text\" i64));", 0),
            (
                "inv.counter(\"a\", v.map_or(-1, |b| i64::try_from(b).unwrap_or(i64::MAX)));",
                0,
            ),
            ("let s = \"inv.counter(\\\"a\\\", n as i64)\";", 0),
        ];
        for (src, expected) in cases {
            assert_eq!(casts(&Source::new(src.to_owned())).len(), expected, "{src}");
        }
    }

    #[test]
    fn a_cast_reports_the_call_line() {
        let found = casts(&Source::new(
            "\n\ninv\n.counter(\"a\", n as i64);".to_owned(),
        ));
        assert_eq!(found, [(4, "\"a\" , n as i64".to_owned())]);
    }
}
