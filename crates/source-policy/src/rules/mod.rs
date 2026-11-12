//! One module per rule. Each exposes `errors(&Corpus) -> Vec<String>` and
//! carries its own in-memory fixtures.

pub mod counters;
pub mod detached_threads;
pub mod duration_literals;
pub mod entropy;
pub mod io_sizes;
pub mod library_text;
pub mod lifecycle;
pub mod statics;
pub mod test_imports;
pub mod test_modules;
pub mod timed_waits;

use crate::Corpus;

/// The prefix selects at least one file; a layout change that empties it
/// must fail rather than audit nothing.
pub(crate) fn require_files(corpus: &Corpus, prefix: &str, rule: &str) -> Option<String> {
    corpus
        .under(prefix)
        .next()
        .is_none()
        .then(|| format!("{rule}: no Rust file under {prefix}"))
}

/// The listed files are present in the corpus.
pub(crate) fn require_sentinels(corpus: &Corpus, paths: &[&str], rule: &str) -> Vec<String> {
    paths
        .iter()
        .filter(|path| !corpus.files.contains_key(**path))
        .map(|path| format!("{rule}: corpus sentinel missing: {path}"))
        .collect()
}
