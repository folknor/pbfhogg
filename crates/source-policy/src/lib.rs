//! The repository's source-policy audits, over one tokenized corpus.
//!
//! Every rule reads the same [`Corpus`]: each Rust file under a member's
//! `src/` and `tests/` read and tokenized once. Lexical rules work on the
//! tokens alone, so a file the parser cannot read still gets them; the
//! structural reads (the module graph, which the test-module audit and the
//! library static inventory walk, item owners, and the test-only exemptions
//! the library text and ID sign rules share) also parse with syn and fail
//! on a file it cannot parse. None of this is macro expansion: what a rule
//! promises about
//! macro input is stated on the rule.
//!
//! The rules run as this package's unit tests, in the `all` sweep only (see
//! the manifest); the real tree is audited by one test per rule
//! (`tree::<rule>`), all reading one corpus built once, so each rule's
//! audit is bounded on its own and the rules run in parallel.

// Reads the repository's own source files, not an operator's inputs.
#![allow(clippy::disallowed_methods)]

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

pub mod lex;
pub mod rules;

use lex::Source;

/// Every Rust file under each workspace member's `src/` and `tests/`, keyed
/// by repository-relative path with `/` separators.
pub struct Corpus {
    pub root: PathBuf,
    pub files: BTreeMap<String, Source>,
}

impl Corpus {
    pub fn load(root: &Path) -> Result<Self, String> {
        let root = &normalize(root);
        let mut files = BTreeMap::new();
        let crates = root.join("crates");
        for member in sorted_entries(&crates)? {
            for tree in ["src", "tests"] {
                let dir = member.join(tree);
                if !dir.is_dir() {
                    continue;
                }
                let mut paths = Vec::new();
                rust_files(&dir, &mut paths)?;
                for path in paths {
                    let text = read(&path)?;
                    files.insert(relative(root, &path), Source::new(text));
                }
            }
        }
        Ok(Self {
            root: root.clone(),
            files,
        })
    }

    /// An in-memory corpus, for fixtures.
    pub fn from_sources<'a>(sources: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        Self {
            root: PathBuf::from("/fixture"),
            files: sources
                .into_iter()
                .map(|(path, text)| (path.to_owned(), Source::new(text.to_owned())))
                .collect(),
        }
    }

    /// The files whose path starts with `prefix`, in path order.
    pub fn under<'a>(&'a self, prefix: &'a str) -> impl Iterator<Item = (&'a str, &'a Source)> {
        self.files
            .range(prefix.to_owned()..)
            .take_while(move |(path, _)| path.starts_with(prefix))
            .map(|(path, source)| (path.as_str(), source))
    }
}

/// One rule: its name and its audit.
pub type Rule = (&'static str, fn(&Corpus) -> Vec<String>);

/// Declares the rule list once: [`RULES`], which [`audit`] runs, and one
/// real-tree test per rule (`tree::<rule>`) over a corpus shared by all of
/// them, so a rule cannot be listed without being tested and each rule's
/// audit runs, and is bounded, on its own.
macro_rules! rules {
    ($($rule:ident),* $(,)?) => {
        /// Every rule, in report order.
        pub const RULES: &[Rule] = &[$((stringify!($rule), rules::$rule::errors)),*];

        #[cfg(test)]
        mod tree {
            $(
                #[test]
                fn $rule() {
                    super::tree_satisfies(stringify!($rule), crate::rules::$rule::errors);
                }
            )*
        }
    };
}

rules!(
    counters,
    statics,
    lifecycle,
    entropy,
    io_sizes,
    test_imports,
    test_modules,
    library_text,
    detached_threads,
    timed_waits,
    loop_waits,
    duration_literals,
    host_independence,
    environment,
    id_signs,
);

/// Every finding of every rule over the tree at `root`.
pub fn audit(root: &Path) -> Vec<String> {
    let corpus = match Corpus::load(root) {
        Ok(corpus) => corpus,
        Err(error) => return vec![error],
    };
    RULES.iter().flat_map(|(_, rule)| rule(&corpus)).collect()
}

/// The real tree, loaded once for every `tree::<rule>` test.
#[cfg(test)]
fn tree_satisfies(name: &str, rule: fn(&Corpus) -> Vec<String>) {
    static TREE: std::sync::OnceLock<Result<Corpus, String>> = std::sync::OnceLock::new();
    let corpus = TREE.get_or_init(|| Corpus::load(pbfhogg_test_scratch::repository_root()));
    let corpus = match corpus {
        Ok(corpus) => corpus,
        Err(error) => panic!("source policy {name}: {error}"),
    };
    let found = rule(corpus);
    assert!(
        found.is_empty(),
        "source policy {name}:\n{}",
        found.join("\n")
    );
}

pub(crate) fn read(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))
}

fn sorted_entries(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut entries = std::fs::read_dir(dir)
        .map_err(|e| format!("read {}: {e}", dir.display()))?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read {}: {e}", dir.display()))?;
    entries.sort();
    Ok(entries)
}

/// Every `.rs` file under `dir`, recursively, in path order.
pub(crate) fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    for path in sorted_entries(dir)? {
        if path.is_dir() {
            rust_files(&path, out)?;
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

/// `path` relative to `root`, `/`-separated.
pub(crate) fn relative(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// `path` with `.` and `..` resolved lexically (the tree holds no symlinks
/// the audits follow).
pub(crate) fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}
