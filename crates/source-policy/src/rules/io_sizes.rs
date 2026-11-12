//! No literal integer expression in `crates/lib/src/read` or `write`
//! evaluates to `IO_BUF_BYTES`, the shared transfer size owned by
//! `crates/lib/src/tunables.rs`.
//!
//! Lexical: it evaluates maximal integer-only token runs (literals, parens
//! and `+ - * << >> & ^ |` with Rust precedence), not Rust const
//! evaluation. Named aliases, casts and macro-built arithmetic are outside
//! its promise. A run is evaluated whole, so `256 * 1024 * 1024` is not
//! mistaken for a transfer buffer.

use crate::Corpus;
use crate::lex::{Kind, Source, Tok};

const OWNER: &str = "crates/lib/src/tunables.rs";
const TREES: [&str; 2] = ["crates/lib/src/read/", "crates/lib/src/write/"];
const SENTINELS: [&str; 5] = [
    "crates/lib/src/read/blob.rs",
    "crates/lib/src/read/reader.rs",
    "crates/lib/src/write/writer.rs",
    "crates/lib/src/write/direct_writer.rs",
    "crates/lib/src/write/uring_writer.rs",
];

pub fn errors(corpus: &Corpus) -> Vec<String> {
    let mut found = super::require_sentinels(corpus, &SENTINELS, "io sizes");
    let size = match corpus.files.get(OWNER).map(owner_size) {
        Some(Ok(size)) => size,
        Some(Err(error)) => return vec![format!("{OWNER}: {error}")],
        None => return vec![format!("io sizes: {OWNER} missing")],
    };
    for tree in TREES {
        for (path, source) in corpus.under(tree) {
            for line in duplicate_lines(source, size) {
                found.push(format!(
                    "{path}:{line}: use IO_BUF_BYTES for the transfer size"
                ));
            }
        }
    }
    found
}

/// The one `const IO_BUF_BYTES: usize = <integer expression>;` value.
fn owner_size(source: &Source) -> Result<i128, String> {
    let code = source.code();
    let mut values = Vec::new();
    for i in 0..code.len() {
        let shape = ["const", "IO_BUF_BYTES", ":", "usize", "="];
        let matches = shape
            .iter()
            .enumerate()
            .all(|(k, text)| code.get(i + k).is_some_and(|t| source.text_of(t) == *text));
        if !matches {
            continue;
        }
        let rest = &code[i + shape.len()..];
        let end = rest
            .iter()
            .position(|t| source.is_punct(t, ";"))
            .ok_or("unterminated IO_BUF_BYTES")?;
        let terms = terms(source, &rest[..end]).ok_or("IO_BUF_BYTES is not integer arithmetic")?;
        values.push(evaluate(&terms).ok_or("IO_BUF_BYTES is not integer arithmetic")?);
    }
    match values[..] {
        [size] if size > 0 => Ok(size),
        _ => Err("expected one positive IO_BUF_BYTES owner".to_owned()),
    }
}

/// An integer literal's value, for unsuffixed, `usize` and `uN` literals.
fn integer(text: &str) -> Option<i128> {
    let digits = text.replace('_', "");
    let (radix, body) = match digits.get(..2) {
        Some("0x" | "0X") => (16, &digits[2..]),
        Some("0b" | "0B") => (2, &digits[2..]),
        Some("0o" | "0O") => (8, &digits[2..]),
        _ => (10, digits.as_str()),
    };
    let end = body
        .find(|c: char| !c.is_digit(radix))
        .unwrap_or(body.len());
    let suffix = &body[end..];
    let unsigned = suffix.is_empty()
        || suffix == "usize"
        || suffix
            .strip_prefix('u')
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    if !unsigned || end == 0 {
        return None;
    }
    i128::from_str_radix(&body[..end], radix).ok()
}

/// One element of an integer-only run.
#[derive(Clone, Debug, PartialEq)]
enum Term {
    Number(i128),
    Op(&'static str),
}

const OPS: [&str; 9] = ["(", ")", "+", "-", "*", "&", "^", "|", "<<"];

fn op_of(source: &Source, toks: &[Tok], i: usize) -> Option<(&'static str, usize)> {
    let tok = &toks[i];
    if tok.kind != Kind::Punct {
        return None;
    }
    let text = source.text_of(tok);
    for shift in ["<", ">"] {
        let next = toks.get(i + 1);
        if text == shift && next.is_some_and(|n| n.start == tok.end && source.text_of(n) == shift) {
            return Some((if shift == "<" { "<<" } else { ">>" }, 2));
        }
    }
    OPS.iter().find(|op| **op == text).map(|op| (*op, 1))
}

/// The whole token slice as terms, or `None` if any token is outside them.
fn terms(source: &Source, toks: &[Tok]) -> Option<Vec<Term>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        if toks[i].kind == Kind::Int {
            out.push(Term::Number(integer(source.text_of(&toks[i]))?));
            i += 1;
        } else {
            let (op, width) = op_of(source, toks, i)?;
            out.push(Term::Op(op));
            i += width;
        }
    }
    Some(out)
}

/// Lines of maximal integer-only runs that evaluate to `size`.
pub fn duplicate_lines(source: &Source, size: i128) -> Vec<usize> {
    let code = source.code();
    let mut found = Vec::new();
    let mut run: Vec<Term> = Vec::new();
    let mut run_line = 0;
    let mut i = 0;
    loop {
        let next = code.get(i).and_then(|tok| {
            if tok.kind == Kind::Int {
                integer(source.text_of(tok)).map(|n| (Term::Number(n), 1))
            } else {
                op_of(source, &code, i).map(|(op, w)| (Term::Op(op), w))
            }
        });
        match next {
            Some((term, width)) => {
                if run.is_empty() {
                    run_line = code[i].line;
                }
                run.push(term);
                i += width;
            }
            None => {
                if run_matches(&run, size) {
                    found.push(run_line);
                }
                run.clear();
                if i >= code.len() {
                    break;
                }
                i += 1;
            }
        }
    }
    found
}

fn run_matches(run: &[Term], size: i128) -> bool {
    let mut run = run;
    let count = |r: &[Term], p: &'static str| r.iter().filter(|t| **t == Term::Op(p)).count();
    // Parentheses of an enclosing call can be unbalanced at a lexical
    // boundary: drop only unmatched outer ones.
    while run.first() == Some(&Term::Op("(")) && count(run, "(") > count(run, ")") {
        run = &run[1..];
    }
    while run.last() == Some(&Term::Op(")")) && count(run, ")") > count(run, "(") {
        run = &run[..run.len() - 1];
    }
    !run.is_empty() && evaluate(run) == Some(size)
}

/// Evaluate a whole term list with Rust precedence, `None` if it is not one
/// well-formed expression or leaves the audited subset.
fn evaluate(terms: &[Term]) -> Option<i128> {
    let mut parser = Parser { terms, at: 0 };
    let value = parser.binary(0)?;
    (parser.at == terms.len()).then_some(value)
}

struct Parser<'a> {
    terms: &'a [Term],
    at: usize,
}

/// Binary operators by ascending precedence level.
const LEVELS: [&[&str]; 5] = [&["|"], &["^"], &["&"], &["<<", ">>"], &["+", "-"]];

impl Parser<'_> {
    fn binary(&mut self, level: usize) -> Option<i128> {
        let Some(ops) = LEVELS.get(level) else {
            return self.product();
        };
        let mut left = self.binary(level + 1)?;
        while let Some(Term::Op(op)) = self.terms.get(self.at) {
            if !ops.contains(op) {
                break;
            }
            let op = *op;
            self.at += 1;
            let right = self.binary(level + 1)?;
            left = apply(op, left, right)?;
        }
        Some(left)
    }

    fn product(&mut self) -> Option<i128> {
        let mut left = self.atom()?;
        while self.terms.get(self.at) == Some(&Term::Op("*")) {
            self.at += 1;
            left = left.checked_mul(self.atom()?)?;
        }
        Some(left)
    }

    fn atom(&mut self) -> Option<i128> {
        match self.terms.get(self.at)? {
            Term::Number(n) => {
                self.at += 1;
                Some(*n)
            }
            Term::Op("(") => {
                self.at += 1;
                let value = self.binary(0)?;
                (self.terms.get(self.at) == Some(&Term::Op(")"))).then_some(())?;
                self.at += 1;
                Some(value)
            }
            Term::Op(_) => None,
        }
    }
}

fn apply(op: &str, left: i128, right: i128) -> Option<i128> {
    match op {
        "|" => Some(left | right),
        "^" => Some(left ^ right),
        "&" => Some(left & right),
        "+" => left.checked_add(right),
        "-" => left.checked_sub(right),
        "<<" | ">>" => {
            let shift = u32::try_from(right).ok().filter(|s| *s <= 64)?;
            if op == "<<" {
                left.checked_shl(shift)
            } else {
                left.checked_shr(shift)
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::duplicate_lines;
    use crate::lex::Source;

    fn lines(src: &str, size: i128) -> Vec<usize> {
        duplicate_lines(&Source::new(src.to_owned()), size)
    }

    #[test]
    fn literal_copies_of_the_size_are_found() {
        for expression in [
            "256 * 1024",
            "262_144usize",
            "1 << 18",
            "128 * 2048",
            "0x40000",
            "0b1000000000000000000",
            "(128 * 1024) * 2",
        ] {
            assert!(
                !lines(&format!("let size = {expression};"), 262_144).is_empty(),
                "{expression}"
            );
        }
        assert!(!lines("let size = 1 << 17;", 131_072).is_empty());
        assert_eq!(lines("let a = 1;\nlet size = 256 * 1024;", 262_144), [2]);
    }

    #[test]
    fn text_other_values_and_whole_budgets_are_not_copies() {
        for src in [
            "// 256 * 1024\nlet x = IO_BUF_BYTES;",
            "let text = \"262144\";",
            "let text = r#\"1 << 18\"#;",
            "let budget = 256 * 1024 * 1024;",
            "let x = 1262144;",
            "let x = 0x400000;",
            "/* nested /* 262144 */ comment */",
        ] {
            assert!(lines(src, 262_144).is_empty(), "{src}");
        }
    }
}
