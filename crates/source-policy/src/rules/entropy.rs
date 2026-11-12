//! The literal `getrandom` identifier lives only in the scratch entropy
//! adapter, `scratch::random_u128`, exactly once.
//!
//! Lexical ownership only: aliases imported from elsewhere, other entropy
//! APIs and macro-generated identifiers require review. Comments and
//! literals cannot supply a call or an exemption. No production or test file
//! is exempt wholesale. PID acquisition is separate: output publication also
//! uses a PID in its names.

use crate::Corpus;
use crate::lex::{Source, function_body};

pub const OWNER: &str = "crates/lib/src/scratch.rs";
const TREES: [&str; 3] = ["crates/lib/src/", "crates/cli/src/", "crates/harness/src/"];
/// Sentinels pinning every selected source tree.
const MUST_SCAN: [&str; 4] = [
    OWNER,
    "crates/lib/src/read/reader.rs",
    "crates/cli/src/lib.rs",
    "crates/harness/src/main.rs",
];

pub fn errors(corpus: &Corpus) -> Vec<String> {
    let mut found = super::require_sentinels(corpus, &MUST_SCAN, "entropy owner");
    let sources: Vec<(&str, &Source)> = TREES.iter().flat_map(|tree| corpus.under(tree)).collect();
    found.extend(ownership_errors(&sources));
    found
}

fn mentions(source: &Source, toks: &[crate::lex::Tok]) -> Vec<usize> {
    toks.iter()
        .filter(|t| source.name(t) == Some("getrandom"))
        .map(|t| t.line)
        .collect()
}

pub fn ownership_errors(sources: &[(&str, &Source)]) -> Vec<String> {
    let mut found = Vec::new();
    let mut owner_seen = false;
    for (path, source) in sources {
        let lines = mentions(source, &source.code());
        if *path == OWNER {
            owner_seen = true;
            let in_body =
                function_body(source, "random_u128").map(|body| mentions(source, &body).len());
            if in_body != Some(1) {
                found.push(format!(
                    "{path}: random_u128 must own one getrandom acquisition"
                ));
            }
            if lines.len() != 1 {
                found.push(format!("{path}: getrandom belongs only in random_u128"));
            }
        } else {
            for line in lines {
                found.push(format!(
                    "{path}:{line}: getrandom belongs in scratch::random_u128"
                ));
            }
        }
    }
    if !owner_seen {
        found.push(format!("{OWNER}: entropy owner missing"));
    }
    found
}

#[cfg(test)]
mod tests {
    use super::{OWNER, ownership_errors};
    use crate::lex::Source;

    fn errors(sources: &[(&str, &str)]) -> Vec<String> {
        let owned: Vec<(&str, Source)> = sources
            .iter()
            .map(|(p, s)| (*p, Source::new((*s).to_owned())))
            .collect();
        let refs: Vec<(&str, &Source)> = owned.iter().map(|(p, s)| (*p, s)).collect();
        ownership_errors(&refs)
    }

    const GOOD: &str = "fn random_u128() { unsafe { libc::getrandom(p, n, 0) }; }";

    #[test]
    fn the_owner_alone_passes_and_text_never_counts() {
        assert!(errors(&[(OWNER, GOOD)]).is_empty());
        let other = "
            // libc::getrandom(p, n, 0)
            /* nested /* getrandom */ comment */
            const TEXT: &str = r###\"getrandom\"###;
        ";
        assert!(errors(&[(OWNER, GOOD), ("other.rs", other)]).is_empty());
    }

    #[test]
    fn a_misplaced_or_missing_acquisition_fails() {
        let stray = format!("{GOOD}\nfn stray() {{ libc::getrandom(p,n,0); }}");
        let twice = format!("{GOOD}\nfn random_u128() {{}}");
        for bad in [
            "fn random_u128() {}",
            stray.as_str(),
            "fn random_u128() {} fn stray() { libc::getrandom(p,n,0); }",
            twice.as_str(),
            "fn random_u128() { let s = \"getrandom\"; }",
        ] {
            assert!(!errors(&[(OWNER, bad)]).is_empty(), "{bad}");
        }
        for bad in [
            "libc::getrandom(p,n,0);",
            "use libc::getrandom as entropy;",
            "libc::\n getrandom(p,n,0);",
        ] {
            assert!(
                !errors(&[(OWNER, GOOD), ("other.rs", bad)]).is_empty(),
                "{bad}"
            );
        }
        assert!(!errors(&[]).is_empty());
    }
}
