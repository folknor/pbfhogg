//! Every Rust file under a member's `tests/` is reachable from a test target
//! some configured sweep builds, and every module declared in one is
//! enabled in at least one sweep.
//!
//! The graph follows `mod` declarations, `#[path]` attributes and `cfg`
//! gates, evaluated against each sweep in `brokkr.d/checks.toml` (its
//! feature closure per member manifest, its target environment and its
//! `--cfg` rustflags), never against this crate's own build. External
//! modules must be at file scope; inline modules may contain tests but not
//! external declarations. Unknown cfg forms, `cfg_attr` on a module and a
//! literally named `mod` the parser did not see as a module item (macro
//! input, a function body) fail closed; a module named by a macro
//! metavariable is outside the promise: extend the audit deliberately when the test graph needs a new
//! form. This checks source reachability and module presence, not macro
//! expansion or whether a test executes.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::{Attribute, Expr, Item, Lit, Meta, Token};

use crate::lex::Source;
use crate::{Corpus, normalize, relative};

/// Bare compiler cfgs the project declares in its check-cfg list. One is
/// true in a sweep only when that sweep's rustflags pass `--cfg NAME`.
pub const CUSTOM_CFGS: [&str; 1] = ["pbfhogg_test_hooks"];

/// One module declaration: where, what, its own gates and path.
#[derive(Clone)]
struct Decl {
    line: usize,
    name: String,
    gates: Vec<Meta>,
    path: Option<String>,
    /// Inline content; `None` for an external `mod name;`.
    inline: Option<Vec<Decl>>,
}

/// What the audit reads from one file.
#[derive(Clone)]
struct Shape {
    file_gates: Vec<Meta>,
    decls: Vec<Decl>,
}

pub fn errors(corpus: &Corpus) -> Vec<String> {
    match audit(corpus) {
        Ok(()) => Vec::new(),
        Err(error) => vec![format!("test modules: {error}")],
    }
}

// ---- cfg evaluation -------------------------------------------------------

/// The value of a cfg predicate under `features` (features, the target
/// environment `gnu`/`musl` and the custom cfgs a sweep sets).
pub fn cfg_value(meta: &Meta, features: &BTreeSet<String>) -> Result<bool, String> {
    let name = meta
        .path()
        .get_ident()
        .map(ToString::to_string)
        .ok_or("unsupported cfg path")?;
    match meta {
        Meta::List(list) => {
            let args = list
                .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
                .map_err(|e| format!("unsupported cfg {name}: {e}"))?;
            // Evaluate every argument, even after the outcome is known, so
            // unknown syntax is always refused.
            let values = args
                .iter()
                .map(|arg| cfg_value(arg, features))
                .collect::<Result<Vec<_>, _>>()?;
            match name.as_str() {
                "all" => Ok(values.iter().all(|v| *v)),
                "any" => Ok(values.iter().any(|v| *v)),
                "not" if values.len() == 1 => Ok(!values[0]),
                _ => Err(format!("unsupported cfg: {name}(..)")),
            }
        }
        Meta::Path(_) => match name.as_str() {
            "test" | "unix" => Ok(true),
            custom if CUSTOM_CFGS.contains(&custom) => Ok(features.contains(custom)),
            _ => Err(format!("unsupported cfg: {name}")),
        },
        Meta::NameValue(pair) => {
            let Expr::Lit(syn::ExprLit {
                lit: Lit::Str(value),
                ..
            }) = &pair.value
            else {
                return Err(format!("unsupported cfg value for {name}"));
            };
            let value = value.value();
            match name.as_str() {
                "feature" | "target_env" => Ok(features.contains(&value)),
                "target_endian" => Ok(value == "little"),
                "target_arch" => Ok(value == "x86_64"),
                // The executable sweeps configured by this project are Linux.
                "target_os" => Ok(value == "linux"),
                _ => Err(format!("unsupported cfg: {name}")),
            }
        }
    }
}

fn all_true(gates: &[Meta], features: &BTreeSet<String>) -> Result<bool, String> {
    let values = gates
        .iter()
        .map(|gate| cfg_value(gate, features))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(values.iter().all(|v| *v))
}

/// The custom cfgs a sweep's rustflags set, as `--cfg NAME` or `--cfg=NAME`.
pub fn sweep_cfgs(rustflags: &[String]) -> Result<BTreeSet<String>, String> {
    let mut found = BTreeSet::new();
    for (i, flag) in rustflags.iter().enumerate() {
        if flag == "--cfg" {
            found.insert(
                rustflags
                    .get(i + 1)
                    .ok_or("trailing --cfg in sweep rustflags")?
                    .clone(),
            );
        } else if let Some(name) = flag.strip_prefix("--cfg=") {
            found.insert(name.to_owned());
        }
    }
    Ok(found
        .into_iter()
        .filter(|n| CUSTOM_CFGS.contains(&n.as_str()))
        .collect())
}

// ---- reading one file -----------------------------------------------------

fn gates_of(attrs: &[Attribute], inner: bool) -> Result<(Vec<Meta>, Option<String>), String> {
    let mut gates = Vec::new();
    let mut paths = Vec::new();
    for attr in attrs {
        let is_inner = matches!(attr.style, syn::AttrStyle::Inner(_));
        if is_inner != inner {
            continue;
        }
        if attr.path().is_ident("cfg_attr") {
            return Err(format!(
                "line {}: cfg_attr modules need explicit audit support",
                line(attr)
            ));
        }
        if attr.path().is_ident("cfg") {
            gates.push(
                attr.parse_args::<Meta>()
                    .map_err(|e| format!("line {}: cfg: {e}", line(attr)))?,
            );
        }
        if attr.path().is_ident("path") {
            let Meta::NameValue(pair) = &attr.meta else {
                return Err(format!("line {}: unsupported path attribute", line(attr)));
            };
            let Expr::Lit(syn::ExprLit {
                lit: Lit::Str(value),
                ..
            }) = &pair.value
            else {
                return Err(format!("line {}: unsupported path attribute", line(attr)));
            };
            paths.push(value.value());
        }
    }
    if paths.len() > 1 {
        return Err("more than one path attribute on a module".to_owned());
    }
    Ok((gates, paths.pop()))
}

fn line(node: &impl Spanned) -> usize {
    node.span().start().line
}

fn decls(items: &[Item], nested: bool) -> Result<Vec<Decl>, String> {
    let mut out = Vec::new();
    for item in items {
        let Item::Mod(module) = item else {
            continue;
        };
        let (mut gates, path) = gates_of(&module.attrs, false)?;
        let inline = match &module.content {
            None if nested => {
                return Err(format!(
                    "line {}: external module inside inline body needs explicit audit support",
                    line(&module.mod_token)
                ));
            }
            None => None,
            Some((_, items)) => {
                let (inner, _) = gates_of(&module.attrs, true)?;
                gates.extend(inner);
                Some(decls(items, true)?)
            }
        };
        out.push(Decl {
            line: line(&module.mod_token),
            name: module.ident.to_string(),
            gates,
            path,
            inline,
        });
    }
    Ok(out)
}

fn count_decls(decls: &[Decl]) -> usize {
    decls
        .iter()
        .map(|d| 1 + d.inline.as_deref().map_or(0, count_decls))
        .sum()
}

/// Read one file's module shape. Every `mod` keyword token must be a
/// declaration the parser reached at item level: one inside macro input or
/// a function body is a module this audit cannot place.
fn shape(source: &Source) -> Result<Shape, String> {
    let file = syn::parse_file(&source.text).map_err(|e| {
        let at = e.span().start();
        format!("line {}: does not parse: {e}", at.line)
    })?;
    let (file_gates, _) = gates_of(&file.attrs, true)?;
    let decls = decls(&file.items, false)?;
    // A `mod $name` built from a macro metavariable names no module the
    // source can place and is outside the promise, as macro expansion is.
    let code = source.code();
    let keywords = code
        .windows(2)
        .filter(|w| source.is_word(&w[0], "mod") && source.name(&w[1]).is_some())
        .count();
    if keywords != count_decls(&decls) {
        return Err(format!(
            "{keywords} `mod` keywords but {} module items: a module inside macro input \
             or a function body needs explicit audit support",
            count_decls(&decls)
        ));
    }
    Ok(Shape { file_gates, decls })
}

// ---- the graph ------------------------------------------------------------

/// Shapes by absolute path, read through the corpus once each.
struct Shapes<'a> {
    corpus: &'a Corpus,
    cache: HashMap<PathBuf, Result<Shape, String>>,
}

impl Shapes<'_> {
    fn get(&mut self, path: &Path) -> Result<Shape, String> {
        if let Some(found) = self.cache.get(path) {
            return found.clone();
        }
        let key = relative(&self.corpus.root, path);
        let result = match self.corpus.files.get(&key) {
            Some(source) => shape(source),
            None => Err("declared module missing".to_owned()),
        }
        .map_err(|e| format!("{key}: {e}"));
        self.cache.insert(path.to_path_buf(), result.clone());
        result
    }

    fn exists(&self, path: &Path) -> bool {
        self.corpus
            .files
            .contains_key(&relative(&self.corpus.root, path))
    }

    /// Files reachable from `roots`, skipping those whose inner gates fail.
    fn reachable(
        &mut self,
        roots: &[PathBuf],
        features: &BTreeSet<String>,
    ) -> Result<BTreeSet<PathBuf>, String> {
        let mut visited = BTreeSet::new();
        let mut pending = roots.to_vec();
        while let Some(path) = pending.pop() {
            if visited.contains(&path) {
                continue;
            }
            let shape = self.get(&path)?;
            if !all_true(&shape.file_gates, features)? {
                continue;
            }
            visited.insert(path.clone());
            let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
            let base = if path.file_name().is_some_and(|n| n == "mod.rs") || roots.contains(&path) {
                parent.clone()
            } else {
                path.with_extension("")
            };
            for decl in &shape.decls {
                // Evaluate every gate, even after a false one, to reject
                // unknown syntax.
                if decl.inline.is_some() || !all_true(&decl.gates, features)? {
                    continue;
                }
                pending.push(self.edge(decl, &base, &parent)?);
            }
        }
        Ok(visited)
    }

    /// A `#[path]` resolves beside the declaring file (Rust reference, path
    /// attribute), which differs from `base` for a non-mod.rs file; a
    /// default name resolves under `base`.
    fn edge(&self, decl: &Decl, base: &Path, directory: &Path) -> Result<PathBuf, String> {
        if let Some(path) = &decl.path {
            return Ok(normalize(&directory.join(path)));
        }
        let candidates = [
            base.join(format!("{}.rs", decl.name)),
            base.join(&decl.name).join("mod.rs"),
        ];
        let existing: Vec<&PathBuf> = candidates.iter().filter(|p| self.exists(p)).collect();
        match existing[..] {
            [one] => Ok(normalize(one)),
            _ => Err(format!(
                "module {}: expected exactly one of {}",
                decl.name,
                candidates
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }
}

/// (line, name) of every declaration, and of those enabled under `features`
/// (their own gates and every enclosing inline module's).
fn presence(
    decls: &[Decl],
    features: &BTreeSet<String>,
    active: bool,
    out: &mut Seen,
) -> Result<(), String> {
    for decl in decls {
        let on = all_true(&decl.gates, features)? && active;
        out.0.insert((decl.line, decl.name.clone()));
        if on {
            out.1.insert((decl.line, decl.name.clone()));
        }
        if let Some(inner) = &decl.inline {
            presence(inner, features, on, out)?;
        }
    }
    Ok(())
}

type Identity = (PathBuf, usize, String);

/// Declarations as (line, name): every one, and those enabled.
type Seen = (BTreeSet<(usize, String)>, BTreeSet<(usize, String)>);

/// A disabled declaration is a platform alternative when an enabled module
/// of the same name exists in the file; only never-enabled names fail.
fn require_module_coverage(
    declared: &BTreeSet<Identity>,
    enabled: &BTreeSet<Identity>,
) -> Result<(), String> {
    let enabled_names: BTreeSet<(&PathBuf, &String)> =
        enabled.iter().map(|(p, _, n)| (p, n)).collect();
    let missing: Vec<String> = declared
        .difference(enabled)
        .filter(|(p, _, n)| !enabled_names.contains(&(p, n)))
        .map(|(p, line, n)| format!("{}:{line}: mod {n}", p.display()))
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Test module declarations outside executable sweep shapes:\n{}",
            missing.join("\n")
        ))
    }
}

// ---- configuration --------------------------------------------------------

fn toml_file(root: &Path, rel: &str) -> Result<toml::Table, String> {
    crate::read(&root.join(rel))?
        .parse::<toml::Table>()
        .map_err(|e| format!("{rel}: {e}"))
}

fn strings(value: Option<&toml::Value>) -> Vec<String> {
    value
        .and_then(toml::Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// One sweep's evaluation set for one member: the feature closure under
/// the member's manifest, the target environment and the sweep's cfgs.
fn sweep_features(
    sweep: &toml::Table,
    manifest_features: &toml::Table,
) -> Result<BTreeSet<String>, String> {
    let mut features: BTreeSet<String> = strings(sweep.get("features")).into_iter().collect();
    if !sweep
        .get("no_default_features")
        .and_then(toml::Value::as_bool)
        .unwrap_or(false)
    {
        features.insert("default".to_owned());
    }
    loop {
        let expanded: BTreeSet<String> = features
            .iter()
            .flat_map(|f| strings(manifest_features.get(f)))
            .filter(|v| !v.contains('/') && !v.starts_with("dep:"))
            .chain(features.iter().cloned())
            .collect();
        if expanded == features {
            break;
        }
        features = expanded;
    }
    // Custom cfgs share the evaluator's name set with features, so a
    // feature of the same name (declared, or implicit from an optional
    // dependency and reached by expansion) would credit them without the
    // cfg. Checked after expansion, before the cfgs join.
    let clash: Vec<&String> = features
        .iter()
        .chain(manifest_features.keys())
        .filter(|f| CUSTOM_CFGS.contains(&f.as_str()))
        .collect();
    if !clash.is_empty() {
        return Err(format!("feature named like a custom cfg: {clash:?}"));
    }
    let target = sweep
        .get("target")
        .and_then(toml::Value::as_str)
        .unwrap_or("");
    features.insert(
        if target.ends_with("-musl") {
            "musl"
        } else {
            "gnu"
        }
        .to_owned(),
    );
    features.extend(sweep_cfgs(&strings(sweep.get("rustflags")))?);
    Ok(features)
}

fn audit(corpus: &Corpus) -> Result<(), String> {
    let root = corpus.root.as_path();
    let checks = toml_file(root, "brokkr.d/checks.toml")?;
    let sweeps: Vec<&toml::Table> = checks
        .get("check")
        .and_then(toml::Value::as_array)
        .map(|a| a.iter().filter_map(toml::Value::as_table).collect())
        .unwrap_or_default();
    let workspace = toml_file(root, "Cargo.toml")?;
    let members = strings(workspace.get("workspace").and_then(|w| w.get("members")));
    let mut shapes = Shapes {
        corpus,
        cache: HashMap::new(),
    };
    let mut found = BTreeSet::new();
    let mut covered = BTreeSet::new();
    let mut enabled = BTreeSet::new();
    for member in &members {
        let manifest = toml_file(root, &format!("{member}/Cargo.toml"))?;
        let package = manifest
            .get("package")
            .and_then(toml::Value::as_table)
            .ok_or(format!("{member}: no package"))?;
        if package.get("autotests").and_then(toml::Value::as_bool) != Some(false) {
            return Err(format!(
                "{member}: audit expects explicit test targets (autotests = false)"
            ));
        }
        let name = package
            .get("name")
            .and_then(toml::Value::as_str)
            .unwrap_or("");
        let prefix = format!("{member}/tests/");
        found.extend(corpus.under(&prefix).map(|(p, _)| root.join(p)));
        let empty = toml::Table::new();
        let manifest_features = manifest
            .get("features")
            .and_then(toml::Value::as_table)
            .unwrap_or(&empty);
        let targets: Vec<&toml::Table> = manifest
            .get("test")
            .and_then(toml::Value::as_array)
            .map(|a| a.iter().filter_map(toml::Value::as_table).collect())
            .unwrap_or_default();
        for sweep in sweeps
            .iter()
            .filter(|s| strings(s.get("packages")).iter().any(|p| p == name))
        {
            let features =
                sweep_features(sweep, manifest_features).map_err(|e| format!("{member}: {e}"))?;
            for target in &targets {
                if !strings(target.get("required-features"))
                    .iter()
                    .all(|f| features.contains(f))
                {
                    continue;
                }
                let target_name = target
                    .get("name")
                    .and_then(toml::Value::as_str)
                    .unwrap_or("");
                let path = target
                    .get("path")
                    .and_then(toml::Value::as_str)
                    .map_or_else(|| format!("tests/{target_name}.rs"), str::to_owned);
                let graph =
                    shapes.reachable(&[normalize(&root.join(member).join(path))], &features)?;
                for file in &graph {
                    let mut seen = (BTreeSet::new(), BTreeSet::new());
                    presence(&shapes.get(file)?.decls, &features, true, &mut seen)?;
                    enabled.extend(seen.1.into_iter().map(|(l, n)| (file.clone(), l, n)));
                }
                covered.extend(graph);
            }
        }
    }
    let missing: Vec<String> = found
        .difference(&covered)
        .map(|p| p.display().to_string())
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "Rust test files outside executable sweep module graphs:\n{}",
            missing.join("\n")
        ));
    }
    let mut declared = BTreeSet::new();
    for file in &found {
        let mut seen = (BTreeSet::new(), BTreeSet::new());
        presence(&shapes.get(file)?.decls, &BTreeSet::new(), true, &mut seen)?;
        declared.extend(seen.0.into_iter().map(|(l, n)| (file.clone(), l, n)));
    }
    require_module_coverage(&declared, &enabled)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    use super::{
        Identity, Seen, Shapes, cfg_value, presence, require_module_coverage, shape, sweep_cfgs,
    };
    use crate::Corpus;
    use crate::lex::Source;

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    fn cfg(text: &str, features: &[&str]) -> Result<bool, String> {
        let meta = syn::parse_str(text).map_err(|e| e.to_string())?;
        cfg_value(&meta, &set(features))
    }

    fn present(src: &str, features: &[&str]) -> Seen {
        let shape = shape(&Source::new(src.to_owned())).expect("shape");
        let mut out = (BTreeSet::new(), BTreeSet::new());
        presence(&shape.decls, &set(features), true, &mut out).expect("presence");
        out
    }

    fn qualify(entries: &BTreeSet<(usize, String)>) -> BTreeSet<Identity> {
        entries
            .iter()
            .map(|(l, n)| (PathBuf::from("/g.rs"), *l, n.clone()))
            .collect()
    }

    #[test]
    fn cfg_evaluation_is_closed() {
        assert_eq!(
            cfg(
                "all(feature = \"commands\", not(pbfhogg_test_hooks))",
                &["commands"]
            ),
            Ok(true)
        );
        assert_eq!(cfg("pbfhogg_test_hooks", &["commands"]), Ok(false));
        assert_eq!(
            cfg("all(test, pbfhogg_test_hooks)", &["pbfhogg_test_hooks"]),
            Ok(true)
        );
        assert_eq!(cfg("feature = \"new-feature\"", &["commands"]), Ok(false));
        assert_eq!(
            cfg("any(feature = \"a\", feature = \"b\")", &["b"]),
            Ok(true)
        );
        assert!(cfg("unknown", &[]).is_err());
        assert!(cfg("any(test, unknown)", &[]).is_err());
        let flags = |f: &[&str]| sweep_cfgs(&f.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>());
        assert_eq!(
            flags(&["--cfg", "pbfhogg_test_hooks"]),
            Ok(set(&["pbfhogg_test_hooks"]))
        );
        assert_eq!(
            flags(&["--cfg=pbfhogg_test_hooks"]),
            Ok(set(&["pbfhogg_test_hooks"]))
        );
        assert_eq!(flags(&["--cfg", "other"]), Ok(set(&[])));
        assert!(flags(&["--cfg"]).is_err());
    }

    #[test]
    fn a_hook_module_is_covered_only_by_a_hook_sweep() {
        let src = "#[cfg(pbfhogg_test_hooks)]\nmod hooked {}\nmod always {}";
        let (declared, plain) = present(src, &[]);
        let (_, hooked) = present(src, &["pbfhogg_test_hooks"]);
        assert!(require_module_coverage(&qualify(&declared), &qualify(&hooked)).is_ok());
        let error =
            require_module_coverage(&qualify(&declared), &qualify(&plain)).expect_err("refused");
        assert!(error.contains("mod hooked"), "{error}");
    }

    #[test]
    fn inline_modules_inherit_enclosing_gates() {
        let src = "#[cfg(feature = \"outer\")]\nmod outer {\n#[cfg(feature = \"inner\")]\nmod inner {}\nmod inherited {}\n}\nmod always {}";
        let (declared, none) = present(src, &[]);
        assert_eq!(declared.len(), 4);
        assert_eq!(
            none.iter().map(|(_, n)| n.as_str()).collect::<Vec<_>>(),
            ["always"]
        );
        let (_, outer) = present(src, &["outer"]);
        assert_eq!(outer.len(), 3);
        let (_, both) = present(src, &["outer", "inner"]);
        assert_eq!(both, declared);
        let error =
            require_module_coverage(&qualify(&declared), &qualify(&outer)).expect_err("refused");
        assert!(error.contains("mod inner"), "{error}");
        assert_eq!(
            present("// mod fake {}\nconst S: &str = \"mod fake {}\";", &[])
                .0
                .len(),
            0
        );
    }

    #[test]
    fn unsupported_module_shapes_fail_closed() {
        for src in [
            "mod inline { mod child; }",
            "#[cfg_attr(test, path = \"x.rs\")]\nmod x;",
            "macro_rules! m { () => { #[cfg(any())] mod forgotten {} }; }\nm!();",
            "fn f() { mod hidden {} }",
            "#[cfg(unknown)]\nmod x;",
        ] {
            let result = shape(&Source::new(src.to_owned())).and_then(|s| {
                let mut out = (BTreeSet::new(), BTreeSet::new());
                presence(&s.decls, &BTreeSet::new(), true, &mut out)
            });
            assert!(result.is_err(), "{src}");
        }
    }

    fn graph(files: &[(&str, &str)], features: &[&str]) -> Result<BTreeSet<String>, String> {
        let corpus = Corpus::from_sources(files.iter().copied());
        let mut shapes = Shapes {
            corpus: &corpus,
            cache: Default::default(),
        };
        let root = corpus.root.join("t/all.rs");
        shapes
            .reachable(&[root], &set(features))
            .map(|g| g.iter().map(|p| crate::relative(&corpus.root, p)).collect())
    }

    #[test]
    fn the_graph_follows_rust_module_paths_and_gates() {
        assert_eq!(
            graph(&[("t/all.rs", "mod child;"), ("t/child.rs", "")], &[]),
            Ok(set(&["t/all.rs", "t/child.rs"]))
        );
        let files = [
            ("t/all.rs", "mod child;"),
            (
                "t/child.rs",
                "#[path = \"shared.rs\"]\nmod shared;\nmod nested;",
            ),
            ("t/shared.rs", ""),
            ("t/child/nested.rs", ""),
        ];
        assert_eq!(
            graph(&files, &[]),
            Ok(set(&[
                "t/all.rs",
                "t/child.rs",
                "t/shared.rs",
                "t/child/nested.rs"
            ]))
        );
        let gated = [
            ("t/all.rs", "mod child;"),
            ("t/child.rs", "#![cfg(feature = \"never-enabled\")]"),
        ];
        assert_eq!(graph(&gated, &[]), Ok(set(&["t/all.rs"])));
        assert_eq!(
            graph(&gated, &["never-enabled"]),
            Ok(set(&["t/all.rs", "t/child.rs"]))
        );
        let helper = [
            (
                "t/all.rs",
                "#[cfg(feature = \"hooks\")]\n#[path = \"helper.rs\"]\nmod helper;",
            ),
            ("t/helper.rs", ""),
        ];
        assert_eq!(graph(&helper, &[]), Ok(set(&["t/all.rs"])));
        assert_eq!(
            graph(&helper, &["hooks"]),
            Ok(set(&["t/all.rs", "t/helper.rs"]))
        );
        assert!(graph(&[("t/all.rs", "mod child;")], &[]).is_err());
    }
}
