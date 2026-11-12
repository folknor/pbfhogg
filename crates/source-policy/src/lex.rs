//! One tokenization per source file, shared by every rule.
//!
//! Every byte of a file lands in exactly one token of the rustc tokenizer,
//! so comment, literal and lifetime boundaries are the language's own and no
//! rule re-derives them.

use ra_ap_rustc_lexer::{FrontmatterAllowed, LiteralKind, TokenKind, tokenize};

/// What a token is, as far as the rules distinguish.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Whitespace,
    LineComment,
    BlockComment,
    Ident,
    /// `r#name`; [`Source::name`] strips the prefix.
    RawIdent,
    Lifetime,
    /// Any string literal: plain, byte or C, raw or cooked.
    Str,
    /// A char or byte literal.
    Char,
    Int,
    Float,
    /// Punctuation, one character per token, and anything else.
    Punct,
}

/// One token: its kind, byte range and the lines it starts and ends on.
#[derive(Clone, Copy, Debug)]
pub struct Tok {
    pub kind: Kind,
    pub start: usize,
    pub end: usize,
    pub line: usize,
    pub end_line: usize,
}

/// One source file, tokenized.
pub struct Source {
    pub text: String,
    pub toks: Vec<Tok>,
    line_starts: Vec<usize>,
}

fn kind_of(kind: TokenKind) -> Kind {
    match kind {
        TokenKind::Whitespace => Kind::Whitespace,
        TokenKind::LineComment { .. } => Kind::LineComment,
        TokenKind::BlockComment { .. } => Kind::BlockComment,
        TokenKind::Ident => Kind::Ident,
        TokenKind::RawIdent => Kind::RawIdent,
        TokenKind::Lifetime { .. } => Kind::Lifetime,
        TokenKind::Literal { kind, .. } => match kind {
            LiteralKind::Str { .. }
            | LiteralKind::ByteStr { .. }
            | LiteralKind::CStr { .. }
            | LiteralKind::RawStr { .. }
            | LiteralKind::RawByteStr { .. }
            | LiteralKind::RawCStr { .. } => Kind::Str,
            LiteralKind::Char { .. } | LiteralKind::Byte { .. } => Kind::Char,
            LiteralKind::Int { .. } => Kind::Int,
            LiteralKind::Float { .. } => Kind::Float,
        },
        _ => Kind::Punct,
    }
}

impl Source {
    pub fn new(text: String) -> Self {
        let mut line_starts = vec![0];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i + 1);
            }
        }
        let mut toks = Vec::new();
        let mut at = 0;
        for token in tokenize(&text, FrontmatterAllowed::No) {
            let end = at + token.len as usize;
            toks.push(Tok {
                kind: kind_of(token.kind),
                start: at,
                end,
                line: 0,
                end_line: 0,
            });
            at = end;
        }
        let mut source = Self {
            text,
            toks,
            line_starts,
        };
        for i in 0..source.toks.len() {
            let tok = source.toks[i];
            source.toks[i].line = source.line_of(tok.start);
            source.toks[i].end_line = source.line_of(tok.end.saturating_sub(1).max(tok.start));
        }
        source
    }

    /// The 1-based line holding byte `offset`.
    pub fn line_of(&self, offset: usize) -> usize {
        self.line_starts.partition_point(|&s| s <= offset)
    }

    /// The byte offset of a 1-based line and a 0-based char column, as
    /// proc-macro2 reports span locations.
    pub fn offset_of(&self, line: usize, column: usize) -> usize {
        let Some(&start) = self.line_starts.get(line.saturating_sub(1)) else {
            return self.text.len();
        };
        let rest = &self.text[start..];
        start
            + rest
                .char_indices()
                .nth(column)
                .map_or(rest.len(), |(i, _)| i)
    }

    pub fn text_of(&self, tok: &Tok) -> &str {
        &self.text[tok.start..tok.end]
    }

    /// An identifier's name, raw prefix stripped; `None` for other tokens.
    pub fn name(&self, tok: &Tok) -> Option<&str> {
        match tok.kind {
            Kind::Ident => Some(self.text_of(tok)),
            Kind::RawIdent => Some(&self.text_of(tok)[2..]),
            _ => None,
        }
    }

    /// The tokens that are code: no whitespace, no comments.
    pub fn code(&self) -> Vec<Tok> {
        self.toks
            .iter()
            .copied()
            .filter(|t| {
                !matches!(
                    t.kind,
                    Kind::Whitespace | Kind::LineComment | Kind::BlockComment
                )
            })
            .collect()
    }

    /// Whether `tok` is the plain identifier `word`.
    pub fn is_word(&self, tok: &Tok, word: &str) -> bool {
        tok.kind == Kind::Ident && self.text_of(tok) == word
    }

    /// Whether `tok` is the punctuation `text`.
    pub fn is_punct(&self, tok: &Tok, text: &str) -> bool {
        tok.kind == Kind::Punct && self.text_of(tok) == text
    }

    /// Code tokens concatenated without whitespace, every literal reduced to
    /// an opaque `"x"`, so a literal can neither supply nor hide code.
    pub fn compact(&self, toks: &[Tok]) -> String {
        toks.iter()
            .map(|t| match t.kind {
                Kind::Str | Kind::Char => "\"x\"",
                _ => self.text_of(t),
            })
            .collect()
    }
}

/// The code tokens of the one body of `fn name`, between its braces. `None`
/// unless exactly one `fn name` header (optionally generic, then `(`) exists
/// and its body balances. A deliberately closed source shape, not control
/// flow: callers compare or search the whole body.
pub fn function_body(source: &Source, name: &str) -> Option<Vec<Tok>> {
    let code = source.code();
    let headers: Vec<usize> = (0..code.len().saturating_sub(1))
        .filter(|&i| {
            source.is_word(&code[i], "fn")
                && source.name(&code[i + 1]) == Some(name)
                && opens_params(source, &code, i + 2)
        })
        .collect();
    let [header] = headers[..] else {
        return None;
    };
    let open = (header..code.len()).find(|&i| source.is_punct(&code[i], "{"))?;
    let mut depth = 0_usize;
    for (i, tok) in code.iter().enumerate().skip(open) {
        if source.is_punct(tok, "{") {
            depth += 1;
        } else if source.is_punct(tok, "}") {
            depth -= 1;
            if depth == 0 {
                return Some(code[open + 1..i].to_vec());
            }
        }
    }
    None
}

/// Whether `(` follows at `at`, after an optional brace-free `<...>`.
fn opens_params(source: &Source, code: &[Tok], mut at: usize) -> bool {
    if code.get(at).is_some_and(|t| source.is_punct(t, "<")) {
        while let Some(tok) = code.get(at) {
            if source.is_punct(tok, "{") || source.is_punct(tok, "}") {
                return false;
            }
            if source.is_punct(tok, ">") {
                break;
            }
            at += 1;
        }
        at += 1;
    }
    code.get(at).is_some_and(|t| source.is_punct(t, "("))
}
