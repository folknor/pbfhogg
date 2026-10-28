//! The one rule for printing PBF string-table text (tag keys and values,
//! member roles, user names, header strings, file names) in human-readable
//! CLI output: `diff -v`, `inspect --show`, the `inspect` report and
//! `inspect tags`. Machine output (`--json`, export) goes through serde_json
//! instead, and `inspect --get` prints the raw value.

use crate::{MemberId, MemberType};

/// A string printed as a double-quoted literal that is also a valid JSON
/// string: `\"`, `\\`, `\n`, `\r` and `\t`, and every other control
/// character (C0, DEL, C1), U+2028, U+2029 and every `Bidi_Control`
/// character as `\u` plus four lowercase hex digits. Every other character
/// prints as itself, unnormalized. A string-table entry may hold any of
/// these, so without the escapes a newline would split one record across
/// lines (and could forge a further record), and a separator or a quote
/// inside the text would make the line ambiguous. The literal is always
/// complete; nothing truncates it.
pub struct Quoted<'a>(pub &'a str);

impl std::fmt::Display for Quoted<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use std::fmt::Write as _;
        f.write_char('"')?;
        let mut rest = self.0;
        while let Some(pos) = rest.find(needs_escape) {
            f.write_str(&rest[..pos])?;
            let mut chars = rest[pos..].chars();
            let Some(c) = chars.next() else { break };
            match c {
                '"' => f.write_str("\\\"")?,
                '\\' => f.write_str("\\\\")?,
                '\n' => f.write_str("\\n")?,
                '\r' => f.write_str("\\r")?,
                '\t' => f.write_str("\\t")?,
                // Every escaped character is in the BMP.
                c => write!(f, "\\u{:04x}", u32::from(c))?,
            }
            rest = chars.as_str();
        }
        f.write_str(rest)?;
        f.write_char('"')
    }
}

fn needs_escape(c: char) -> bool {
    matches!(
        c,
        '"' | '\\'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{061C}'
            | '\u{200E}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2066}'..='\u{2069}'
    ) || c.is_control()
}

/// A relation member's `type/id`, with an unknown member type's raw value
/// spelled out (`unknown(raw)/id`): the raw type is part of member identity,
/// so two unknown members differing only in it must not print identically.
pub struct MemberLabel(pub MemberId);

impl std::fmt::Display for MemberLabel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self.0.member_type() {
            MemberType::Unknown(raw) => return write!(f, "unknown({raw})/{}", self.0.id()),
            MemberType::Node => "node",
            MemberType::Way => "way",
            MemberType::Relation => "relation",
        };
        write!(f, "{name}/{}", self.0.id())
    }
}

#[cfg(test)]
mod tests {
    use super::Quoted;

    #[test]
    fn quoted_escapes_framing_quotes_and_bidi() {
        let q = |s: &str| Quoted(s).to_string();
        assert_eq!(q(""), "\"\"");
        assert_eq!(q("a=b -> c"), "\"a=b -> c\"");
        assert_eq!(q("caf\u{e9} \u{5d0}"), "\"caf\u{e9} \u{5d0}\"");
        assert_eq!(q("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(q("a\nb\r\tc"), "\"a\\nb\\r\\tc\"");
        assert_eq!(
            q("\0\u{1b}\u{7f}\u{85}"),
            "\"\\u0000\\u001b\\u007f\\u0085\""
        );
        assert_eq!(q("\u{2028}\u{2029}"), "\"\\u2028\\u2029\"");
        assert_eq!(
            q("\u{61c}\u{200e}\u{202e}\u{2069}"),
            "\"\\u061c\\u200e\\u202e\\u2069\""
        );
        // A literal backslash-u spelling is not confused with an escape.
        assert_eq!(q("\\u202e"), "\"\\\\u202e\"");
        // One physical line under every common splitter.
        let s = q("x\n\r\u{1c}\u{1d}\u{1e}\u{85}\u{2028}\u{2029}\u{b}\u{c}y");
        assert_eq!(s.lines().count(), 1);
        assert!(!s.chars().any(char::is_control));
    }
}
