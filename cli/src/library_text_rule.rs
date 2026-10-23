//! The library's runtime text names no CLI subcommand and no long flag.
//!
//! The library is one front end's engine among possible others, and an error
//! or warning that tells the reader to "run `pbfhogg cat`" or "pass
//! `--force`" makes the library's text a dependency of this binary's argument
//! surface - wrong for any other caller, and stale the moment a flag is
//! renamed. The library raises a typed error instead and this front end adds
//! the advice (`exit_code::output_hint`, the diagnostic hints in `host`).
//!
//! The rule is a source scan: every string literal under `src/`, outside
//! comments, attributes and test-only items, is checked for `pbfhogg
//! <subcommand>` (the subcommand names come from the built command tree) and
//! for any long-flag shape `--word`. It lives in this crate because only here
//! is the command tree available to name the subcommands.
//!
//! Test-only means gated by an attribute whose cfg predicate *requires*
//! `test` (`#[test]`, `#[cfg(test)]`, `#[cfg(all(test, ..))]`; never
//! `cfg(any(test, ..))`, `cfg(not(test))` or `cfg_attr`), or a module file
//! declared behind one, with its whole subtree. Inside a macro invocation or
//! a `macro_rules!` definition nothing is exempt: attribute-shaped tokens
//! there are macro input, not gates, and every literal is collected.

// Reads the library's source files, not an operator's inputs.
#![allow(clippy::disallowed_methods)]

use std::path::{Path, PathBuf};

/// Long-flag shapes in library literals that are not this front end's flags.
/// Each entry is (path suffix under `src/`, phrase containing the flag, why);
/// an occurrence is allowed only where the phrase covers it.
const ALLOWED: &[(&str, &str, &str)] = &[(
    "commands/time_filter/mod.rs",
    "osmium --drop-metadata",
    "names a flag of osmium, another tool, as the provenance of stripped metadata",
)];

/// Whether `c` can continue an identifier.
fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The literals of one file and the test-only module files it declares.
#[derive(Default)]
struct Scan {
    literals: Vec<(usize, String)>,
    /// `mod x;` items behind a test gate: the module name and its `#[path]`.
    test_mods: Vec<(String, Option<String>)>,
    file_is_test_only: bool,
}

struct Lexer {
    chars: Vec<char>,
    pos: usize,
    line: usize,
}

impl Lexer {
    fn new(src: &str) -> Self {
        Self {
            chars: src.chars().collect(),
            pos: 0,
            line: 1,
        }
    }

    fn peek(&self, ahead: usize) -> Option<char> {
        self.chars.get(self.pos + ahead).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.chars.get(self.pos).copied()?;
        self.pos += 1;
        if c == '\n' {
            self.line += 1;
        }
        Some(c)
    }

    fn skip_line_comment(&mut self) {
        while let Some(c) = self.peek(0) {
            if c == '\n' {
                break;
            }
            self.bump();
        }
    }

    fn skip_block_comment(&mut self) {
        self.bump();
        self.bump();
        let mut depth = 1;
        while depth > 0 {
            match (self.peek(0), self.peek(1)) {
                (Some('/'), Some('*')) => {
                    self.bump();
                    self.bump();
                    depth += 1;
                }
                (Some('*'), Some('/')) => {
                    self.bump();
                    self.bump();
                    depth -= 1;
                }
                (Some(_), _) => {
                    self.bump();
                }
                (None, _) => break,
            }
        }
    }

    /// At the opening quote of a non-raw string; returns its decoded text.
    fn cooked_string(&mut self) -> String {
        self.bump();
        let mut text = String::new();
        while let Some(c) = self.bump() {
            match c {
                '"' => break,
                '\\' => match self.bump() {
                    Some('n') => text.push('\n'),
                    Some('t') => text.push('\t'),
                    Some('r') => text.push('\r'),
                    Some('0') => text.push('\0'),
                    Some('x') => {
                        let hex: String =
                            [self.bump(), self.bump()].into_iter().flatten().collect();
                        if let Some(c) = u8::from_str_radix(&hex, 16).ok().map(char::from) {
                            text.push(c);
                        }
                    }
                    Some('u') => {
                        let mut hex = String::new();
                        while let Some(c) = self.bump() {
                            match c {
                                '{' | '_' => {}
                                '}' => break,
                                c => hex.push(c),
                            }
                        }
                        if let Some(c) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32)
                        {
                            text.push(c);
                        }
                    }
                    Some('\n') => {
                        while self.peek(0).is_some_and(char::is_whitespace) {
                            self.bump();
                        }
                    }
                    Some(other) => text.push(other),
                    None => break,
                },
                c => text.push(c),
            }
        }
        text
    }

    /// At the `r` of a raw string `r#*"`; returns its text.
    fn raw_string(&mut self) -> String {
        self.bump();
        let mut hashes = 0;
        while self.peek(0) == Some('#') {
            self.bump();
            hashes += 1;
        }
        self.bump();
        let mut text = String::new();
        while let Some(c) = self.bump() {
            if c == '"' && (0..hashes).all(|i| self.peek(i) == Some('#')) {
                for _ in 0..hashes {
                    self.bump();
                }
                break;
            }
            text.push(c);
        }
        text
    }

    /// Whether a raw string starts at the current `r`.
    fn at_raw_string(&self) -> bool {
        if self.peek(0) != Some('r') {
            return false;
        }
        let before = self
            .pos
            .checked_sub(1)
            .and_then(|i| self.chars.get(i))
            .copied();
        let prefix_ok = match before {
            None => true,
            Some('b' | 'c') => {
                let earlier = self
                    .pos
                    .checked_sub(2)
                    .and_then(|i| self.chars.get(i))
                    .copied();
                !earlier.is_some_and(is_ident)
            }
            Some(c) => !is_ident(c),
        };
        if !prefix_ok {
            return false;
        }
        let mut i = 1;
        while self.peek(i) == Some('#') {
            i += 1;
        }
        self.peek(i) == Some('"')
    }

    /// At a `'`: skip a char literal, or just the quote of a lifetime.
    fn quote(&mut self) {
        self.bump();
        if self.peek(0) == Some('\\') {
            while let Some(c) = self.bump() {
                if c == '\'' {
                    break;
                }
                if c == '\\' {
                    self.bump();
                }
            }
        } else if self.peek(1) == Some('\'') {
            self.bump();
            self.bump();
        }
    }

    /// Skip a comment, a literal or a char at the cursor if one starts
    /// there, returning a string literal's line and text. `None` with the
    /// cursor unmoved when none starts here.
    fn trivia(&mut self) -> Option<Option<(usize, String)>> {
        match self.peek(0)? {
            '/' if self.peek(1) == Some('/') => self.skip_line_comment(),
            '/' if self.peek(1) == Some('*') => self.skip_block_comment(),
            '"' => {
                let line = self.line;
                return Some(Some((line, self.cooked_string())));
            }
            'r' if self.at_raw_string() => {
                let line = self.line;
                return Some(Some((line, self.raw_string())));
            }
            '\'' => self.quote(),
            _ => return None,
        }
        Some(None)
    }

    /// At `#`: an attribute `#[..]` or `#![..]`, returned whitespace-free
    /// with whether it is inner. `None`, cursor unmoved, if this `#` is not
    /// an attribute.
    fn attribute(&mut self) -> Option<(String, bool)> {
        let inner = self.peek(1) == Some('!');
        let open = if inner { 2 } else { 1 };
        if self.peek(open) != Some('[') {
            return None;
        }
        for _ in 0..=open {
            self.bump();
        }
        let mut depth = 1;
        let mut text = String::new();
        while let Some(c) = self.peek(0) {
            if c == '"' {
                let s = self.cooked_string();
                text.push('"');
                text.push_str(&s);
                text.push('"');
                continue;
            }
            match c {
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        self.bump();
                        break;
                    }
                }
                _ => {}
            }
            if !c.is_whitespace() {
                text.push(c);
            }
            self.bump();
        }
        Some((text, inner))
    }

    /// At an opening delimiter: consume the balanced group, collecting every
    /// literal in it and interpreting nothing (macro input).
    fn literal_group(&mut self, out: &mut Vec<(usize, String)>) {
        let mut depth = 0_i32;
        while self.peek(0).is_some() {
            if let Some(found) = self.trivia() {
                out.extend(found);
                continue;
            }
            match self.bump() {
                Some('(' | '[' | '{') => depth += 1,
                Some(')' | ']' | '}') => {
                    depth -= 1;
                    if depth <= 0 {
                        return;
                    }
                }
                _ => {}
            }
        }
    }

    /// At an identifier: consume the word, and when it opens a macro
    /// invocation `name!(..)` or definition `macro_rules! name { .. }`,
    /// consume its group too, collecting every literal in it (macro input
    /// is literal-only). Returns whether a group was consumed.
    fn word_then_macro_group(&mut self, out: &mut Vec<(usize, String)>) -> bool {
        let start = self.pos;
        while self.peek(0).is_some_and(is_ident) {
            if self.at_raw_string() {
                break;
            }
            self.bump();
        }
        let word: String = self.chars[start..self.pos].iter().collect();
        let skip_ws = |chars: &[char], mut look: usize| {
            while chars.get(look).is_some_and(|c| c.is_whitespace()) {
                look += 1;
            }
            look
        };
        let mut look = skip_ws(&self.chars, self.pos);
        if word.is_empty() || self.chars.get(look) != Some(&'!') {
            return false;
        }
        look = skip_ws(&self.chars, look + 1);
        if word == "macro_rules" {
            while self.chars.get(look).is_some_and(|c| is_ident(*c)) {
                look += 1;
            }
            look = skip_ws(&self.chars, look);
        }
        if !matches!(self.chars.get(look), Some('(' | '[' | '{')) {
            return false;
        }
        while self.pos < look {
            self.bump();
        }
        self.literal_group(out);
        true
    }

    /// At `{`: skip to its matching `}`, honouring comments and literals.
    fn skip_braces(&mut self) {
        let mut depth = 0;
        while self.peek(0).is_some() {
            if self.trivia().is_some() {
                continue;
            }
            match self.bump() {
                Some('{') => depth += 1,
                Some('}') => {
                    depth -= 1;
                    if depth == 0 {
                        return;
                    }
                }
                _ => {}
            }
        }
    }

    /// Skip the item after a test-only attribute. Returns its text,
    /// whitespace collapsed (to recognise `mod x;`), and any `#[path]` among
    /// its further attributes.
    fn skip_item(&mut self) -> (String, Option<String>) {
        let mut text = String::new();
        let mut path = None;
        let (mut paren, mut bracket) = (0_i32, 0_i32);
        let mut keyword_item = None;
        while let Some(c) = self.peek(0) {
            if self.trivia().is_some() {
                continue;
            }
            match c {
                '#' => match self.attribute() {
                    Some((attr, _)) => path = path_attribute(&attr).or(path),
                    None => {
                        self.bump();
                    }
                },
                '(' => {
                    paren += 1;
                    self.bump();
                }
                ')' if paren == 0 => break,
                ')' => {
                    paren -= 1;
                    self.bump();
                }
                '[' => {
                    bracket += 1;
                    self.bump();
                }
                ']' if bracket == 0 => break,
                ']' => {
                    bracket -= 1;
                    self.bump();
                }
                '}' => break,
                ';' if paren == 0 && bracket == 0 => {
                    self.bump();
                    text.push(';');
                    break;
                }
                ',' if paren == 0 && bracket == 0 && keyword_item == Some(false) => break,
                '{' if paren == 0 && bracket == 0 => {
                    self.skip_braces();
                    break;
                }
                c => {
                    if keyword_item.is_none() && is_ident(c) {
                        let word: String = self.chars[self.pos..]
                            .iter()
                            .take_while(|c| is_ident(**c))
                            .collect();
                        keyword_item = Some(matches!(
                            word.as_str(),
                            "fn" | "mod"
                                | "impl"
                                | "struct"
                                | "enum"
                                | "use"
                                | "const"
                                | "static"
                                | "type"
                                | "trait"
                                | "pub"
                                | "unsafe"
                                | "async"
                                | "extern"
                                | "macro_rules"
                        ));
                    }
                    if text.len() < 200 {
                        if c.is_whitespace() {
                            if !text.ends_with(' ') {
                                text.push(' ');
                            }
                        } else {
                            text.push(c);
                        }
                    }
                    self.bump();
                }
            }
        }
        (text, path)
    }
}

/// The value of a `#[path = "..."]` attribute (whitespace-free text).
fn path_attribute(attr: &str) -> Option<String> {
    attr.strip_prefix("path=\"")
        .and_then(|rest| rest.strip_suffix('"'))
        .map(str::to_owned)
}

/// Split a cfg predicate list at top-level commas.
fn cfg_args(list: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut start) = (0_i32, 0);
    for (i, c) in list.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                out.push(&list[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if start < list.len() {
        out.push(&list[start..]);
    }
    out
}

/// Whether a cfg predicate (whitespace-free) holds only in test builds.
fn requires_test(pred: &str) -> bool {
    if pred == "test" {
        return true;
    }
    let call = |name: &str| {
        pred.strip_prefix(name)
            .and_then(|rest| rest.strip_prefix('('))
            .and_then(|rest| rest.strip_suffix(')'))
    };
    if let Some(list) = call("all") {
        return cfg_args(list).into_iter().any(requires_test);
    }
    if let Some(list) = call("any") {
        let args = cfg_args(list);
        return !args.is_empty() && args.into_iter().all(requires_test);
    }
    false
}

/// Whether an attribute (whitespace-free) gates its item to test builds.
fn is_test_attribute(attr: &str) -> bool {
    attr == "test"
        || attr
            .strip_prefix("cfg(")
            .and_then(|rest| rest.strip_suffix(')'))
            .is_some_and(requires_test)
}

fn scan(src: &str) -> Scan {
    let mut lx = Lexer::new(src);
    let mut out = Scan::default();
    let mut depth = 0_i32;
    // A `#[path]` seen on the item being started, for a test-only `mod x;`.
    let mut pending_path: Option<String> = None;
    while let Some(c) = lx.peek(0) {
        if let Some(found) = lx.trivia() {
            out.literals.extend(found);
            continue;
        }
        match c {
            '#' => match lx.attribute() {
                Some((attr, inner)) if is_test_attribute(&attr) => {
                    if inner {
                        if depth == 0 {
                            out.file_is_test_only = true;
                            return out;
                        }
                        // An inner gate on an inline module: the rest of
                        // its block is test-only.
                        let mut level = 0;
                        while let Some(c) = lx.peek(0) {
                            if lx.trivia().is_some() {
                                continue;
                            }
                            lx.bump();
                            match c {
                                '{' => level += 1,
                                '}' if level == 0 => {
                                    depth -= 1;
                                    break;
                                }
                                '}' => level -= 1,
                                _ => {}
                            }
                        }
                        continue;
                    }
                    let (item, path) = lx.skip_item();
                    let path = path.or(pending_path.take());
                    let words: Vec<&str> = item
                        .trim_end_matches(';')
                        .split(|c: char| !is_ident(c))
                        .filter(|w| !w.is_empty())
                        .collect();
                    if item.ends_with(';')
                        && let Some(at) = words.iter().position(|w| *w == "mod")
                        && let Some(name) = words.get(at + 1)
                        && at + 2 == words.len()
                    {
                        out.test_mods.push(((*name).to_owned(), path));
                    }
                }
                Some((attr, _)) => {
                    if let Some(path) = path_attribute(&attr) {
                        pending_path = Some(path);
                    }
                }
                None => {
                    lx.bump();
                }
            },
            c if is_ident(c) => {
                if lx.word_then_macro_group(&mut out.literals) {
                    pending_path = None;
                }
            }
            '{' => {
                depth += 1;
                lx.bump();
                pending_path = None;
            }
            '}' => {
                depth -= 1;
                lx.bump();
                pending_path = None;
            }
            ';' => {
                lx.bump();
                pending_path = None;
            }
            _ => {
                lx.bump();
            }
        }
    }
    out
}

/// The directory the child modules of `file` live in.
fn module_dir(file: &Path) -> PathBuf {
    let dir = file.parent().unwrap_or(Path::new("."));
    let stem = file.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    if matches!(stem, "lib" | "main" | "mod") {
        dir.to_path_buf()
    } else {
        dir.join(stem)
    }
}

/// The files and subtree a test-only `mod name;` in `file` covers: an entry
/// is excluded when it equals one of these or lies under the directory.
fn excluded_by(file: &Path, name: &str, path: Option<&str>) -> Vec<PathBuf> {
    let dir = file.parent().unwrap_or(Path::new("."));
    match path {
        Some(path) => {
            let target = dir.join(path);
            vec![target.with_extension(""), target]
        }
        None => {
            let base = module_dir(file);
            vec![base.join(format!("{name}.rs")), base.join(name)]
        }
    }
}

/// Every `.rs` file under `dir`, recursively.
fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|entry| entry.expect("directory entry").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// One string literal found in non-test library code.
struct Literal {
    file: PathBuf,
    line: usize,
    text: String,
}

/// Every string literal in non-test library code.
fn library_literals() -> Vec<Literal> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../src");
    let mut files = Vec::new();
    rust_files(&root, &mut files);
    assert!(
        files.len() > 50,
        "found only {} files under {}; the scan is looking in the wrong place",
        files.len(),
        root.display()
    );
    let mut excluded: Vec<PathBuf> = Vec::new();
    let mut found: Vec<(PathBuf, Scan)> = Vec::new();
    for file in files {
        let src = std::fs::read_to_string(&file)
            .unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
        let scan = scan(&src);
        for (name, path) in &scan.test_mods {
            excluded.extend(excluded_by(&file, name, path.as_deref()));
        }
        found.push((file, scan));
    }
    // Component-wise: `x/contract.rs` is not under `x/contract`.
    let is_excluded = |file: &Path| excluded.iter().any(|ex| file.starts_with(ex));
    let mut literals = Vec::new();
    for (file, scan) in found {
        if scan.file_is_test_only || is_excluded(&file) {
            continue;
        }
        for (line, text) in scan.literals {
            literals.push(Literal {
                file: file.clone(),
                line,
                text,
            });
        }
    }
    literals
}

/// Every subcommand name and alias in the built command tree.
fn subcommand_names() -> Vec<String> {
    fn walk(cmd: &clap::Command, out: &mut Vec<String>) {
        for sub in cmd.get_subcommands() {
            out.push(sub.get_name().to_owned());
            out.extend(sub.get_all_aliases().map(str::to_owned));
            walk(sub, out);
        }
    }
    let mut cmd = crate::cli_command();
    cmd.build();
    let mut out = Vec::new();
    walk(&cmd, &mut out);
    out.retain(|name| name != "help");
    out
}

/// The `pbfhogg <subcommand>` phrase in `text`, if any; any run of
/// whitespace may separate the two words.
fn names_subcommand<'a>(text: &str, subcommands: &'a [String]) -> Option<&'a str> {
    text.match_indices("pbfhogg").find_map(|(i, _)| {
        let rest = &text[i + "pbfhogg".len()..];
        let trimmed = rest.trim_start();
        if trimmed.len() == rest.len() {
            return None;
        }
        let word: String = trimmed
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        subcommands
            .iter()
            .find(|name| **name == word)
            .map(String::as_str)
    })
}

/// Every long-flag shape (`--word`) in `text`, with its byte range.
fn long_flags(text: &str) -> Vec<(std::ops::Range<usize>, String)> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut out = Vec::new();
    for (k, &(at, c)) in chars.iter().enumerate() {
        let starts = c == '-'
            && chars.get(k + 1).is_some_and(|(_, c)| *c == '-')
            && chars
                .get(k + 2)
                .is_some_and(|(_, c)| c.is_ascii_lowercase());
        let free =
            k == 0 || !chars[k - 1].1.is_alphanumeric() && !matches!(chars[k - 1].1, '-' | '_');
        if starts && free {
            let flag: String = text[at..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                .collect();
            out.push((at..at + flag.len(), flag));
        }
    }
    out
}

/// Whether the flag occurrence at `range` lies inside an allowed phrase.
fn allowed_at(file: &Path, text: &str, range: &std::ops::Range<usize>) -> Option<usize> {
    ALLOWED.iter().position(|(suffix, phrase, _)| {
        file.ends_with(suffix)
            && text
                .match_indices(phrase)
                .any(|(start, _)| start <= range.start && range.end <= start + phrase.len())
    })
}

#[test]
fn library_text_names_no_subcommand_and_no_flag() {
    let subcommands = subcommand_names();
    assert!(
        subcommands.iter().any(|s| s == "cat"),
        "the command tree lists no `cat`: {subcommands:?}"
    );
    let mut problems = Vec::new();
    let mut allowed_used = vec![false; ALLOWED.len()];
    for literal in library_literals() {
        let at = format!("{}:{}", literal.file.display(), literal.line);
        if let Some(name) = names_subcommand(&literal.text, &subcommands) {
            problems.push(format!("{at}: names `pbfhogg {name}`"));
        }
        for (range, flag) in long_flags(&literal.text) {
            match allowed_at(&literal.file, &literal.text, &range) {
                Some(i) => allowed_used[i] = true,
                None => problems.push(format!("{at}: names `{flag}`")),
            }
        }
    }
    for ((file, phrase, _), used) in ALLOWED.iter().zip(allowed_used) {
        if !used {
            problems.push(format!(
                "allow-list entry ({file}, {phrase:?}) matches nothing; remove it"
            ));
        }
    }
    assert!(
        problems.is_empty(),
        "library text names CLI surface; raise a typed error and let the CLI add the \
         advice:\n{}",
        problems.join("\n")
    );
}

#[test]
fn the_scanner_sees_what_it_must_and_skips_what_it_must() {
    let src = r##"
        // "pbfhogg cat" in a comment
        /* "--force" in a /* nested */ block */
        fn f() -> &'static str { let _c = '"'; "run pbfhogg cat \
            now" }
        const RAW: &str = r#"pass --force"#;
        const ESC: &str = "\x20 --io-uring";
        #[cfg(test)]
        mod tests { const X: &str = "--hidden"; }
        #[cfg(test)]
        mod gated;
        #[path = "elsewhere.rs"]
        #[cfg(test)]
        mod moved;
        #[cfg(all(test, feature = "x"))]
        fn g() -> Result<(), ()> { let _ = "--hidden-too"; Ok(()) }
        #[cfg(not(test))]
        const LIVE: &str = "--live";
        #[cfg(any(test, feature = "y"))]
        const MAYBE: &str = "--maybe";
        #[cfg_attr(test, allow(dead_code))]
        const ATTR: &str = "--attr";
        struct S { #[cfg(test)] a: u8, b: &'static str }
        message!(#[cfg(test)] "--macro-input");
        macro_rules! m { (#[cfg(test)] $v:literal) => { "--macro-body" }; }
        mod inline { #![cfg(test)] const Y: &str = "--inner"; }
        const UNI: &str = "\u{2_d}\u{2d}force";
        const AFTER: &str = "after";
    "##;
    let scan = scan(src);
    let texts: Vec<&str> = scan.literals.iter().map(|(_, t)| t.as_str()).collect();
    assert_eq!(
        texts,
        [
            "run pbfhogg cat now",
            "pass --force",
            "  --io-uring",
            "--live",
            "--maybe",
            "--attr",
            "--macro-input",
            "--macro-body",
            "--force",
            "after"
        ],
    );
    assert_eq!(
        scan.test_mods,
        [
            ("gated".to_owned(), None),
            ("moved".to_owned(), Some("elsewhere.rs".to_owned()))
        ]
    );
    assert_eq!(scan.literals[0].0, 4, "{:?}", scan.literals[0]);
    assert!(!scan.file_is_test_only);
    assert!(scan_is_test_only("#![cfg(test)]\nconst X: &str = \"--x\";"));

    let flags: Vec<String> = long_flags("a --io-uring b --x")
        .into_iter()
        .map(|(_, f)| f)
        .collect();
    assert_eq!(flags, ["--io-uring", "--x"]);
    assert!(long_flags("a---b x--y <!-- c").is_empty());
    let subs = ["cat".to_owned()];
    assert_eq!(
        names_subcommand("e.g. pbfhogg cat --clean", &subs),
        Some("cat")
    );
    assert_eq!(names_subcommand("pbfhogg\n   cat", &subs), Some("cat"));
    assert_eq!(names_subcommand("pbfhogg never deletes", &subs), None);
    assert_eq!(names_subcommand("pbfhogg catalogue", &subs), None);

    // The allow-list covers its own occurrence, not the whole literal.
    let file = Path::new("/x/src/commands/time_filter/mod.rs");
    let text = "stripped (osmium --drop-metadata); pass --force";
    let flagged: Vec<String> = long_flags(text)
        .into_iter()
        .filter(|(range, _)| allowed_at(file, text, range).is_none())
        .map(|(_, f)| f)
        .collect();
    assert_eq!(flagged, ["--force"]);

    // A test-only module's subtree is excluded, nothing beside it.
    let excluded = excluded_by(Path::new("/s/write/mod.rs"), "contract", None);
    assert!(excluded.contains(&PathBuf::from("/s/write/contract.rs")));
    assert!(excluded.contains(&PathBuf::from("/s/write/contract")));
    let excluded = excluded_by(Path::new("/s/a.rs"), "b", Some("../t/b.rs"));
    assert!(excluded.contains(&PathBuf::from("/s/../t/b.rs")));
}

fn scan_is_test_only(src: &str) -> bool {
    scan(src).file_is_test_only
}
