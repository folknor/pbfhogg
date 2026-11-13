//! Dataset-facing port of brokkr's config composition. Unrelated sections
//! remain TOML values; this crate neither validates nor executes their commands.
use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::Error;

/// Select brokkr's user config from already-read environment values.
/// An empty override disables it. Relative XDG paths are ignored.
pub fn user_config_path(
    override_path: Option<&OsStr>,
    xdg_config_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Option<PathBuf> {
    if let Some(path) = override_path {
        return (!path.is_empty()).then(|| PathBuf::from(path));
    }
    let base = xdg_config_home
        .filter(|path| Path::new(path).is_absolute())
        .map(PathBuf::from)
        .or_else(|| {
            home.filter(|path| !path.is_empty())
                .map(|path| PathBuf::from(path).join(".config"))
        })?;
    Some(base.join("brokkr/brokkr.toml"))
}

/// One fully composed project config. Dataset paths are relative to the
/// project root, never to an included file's parent.
#[derive(Debug)]
pub struct Config {
    pub(crate) root: PathBuf,
    pub(crate) table: toml::Table,
}

impl Config {
    /// Load a project and its recursive includes, above the optional user
    /// layer. A missing user file is normal; a missing project/include is not.
    /// The caller supplies the user path selected by [`user_config_path`].
    pub fn load(project_file: &Path, user_file: Option<&Path>) -> Result<Self, Error> {
        // A symlinked config takes includes from its target, but dataset
        // paths still belong to the project directory containing the link.
        let parent = project_file
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let root = std::fs::canonicalize(parent)
            .map_err(|e| Error::message(format!("{}: {e}", parent.display())))?;
        let mut table = toml::Table::new();
        let mut walk = Walk::default();
        if let Some(path) = user_file {
            match std::fs::read_to_string(path) {
                Ok(text) => {
                    let mut user = parse(path, &text)?;
                    for key in user.keys() {
                        if ![
                            "textlint",
                            "textlint_preset",
                            "script_check",
                            "results_server",
                        ]
                        .contains(&key.as_str())
                        {
                            return Err(Error::message(format!(
                                "{}: '{key}' is not allowed in the user-wide config",
                                path.display()
                            )));
                        }
                    }
                    // This private user setting never enters project composition.
                    user.remove("results_server");
                    walk.seen.insert(
                        std::fs::canonicalize(path).map_err(|e| Error::message(e.to_string()))?,
                    );
                    merge(&mut table, user, &mut Vec::new());
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(Error::message(format!("{}: {e}", path.display()))),
            }
        }
        walk.visit(project_file, false, &mut table)?;
        Ok(Self { root, table })
    }

    /// The effective TOML, useful for inspecting precedence without reparsing.
    pub fn composed(&self) -> &toml::Table {
        &self.table
    }

    /// The project directory used for dataset paths.
    pub fn project_root(&self) -> &Path {
        &self.root
    }
}

fn parse(path: &Path, text: &str) -> Result<toml::Table, Error> {
    toml::from_str(text).map_err(|e| Error::message(format!("{}: {e}", path.display())))
}

#[derive(Default)]
struct Walk {
    stack: Vec<PathBuf>,
    seen: HashSet<PathBuf>,
}

impl Walk {
    fn visit(
        &mut self,
        path: &Path,
        included: bool,
        merged: &mut toml::Table,
    ) -> Result<(), Error> {
        let at = |message: String| Error::message(format!("{}: {message}", path.display()));
        let canonical = std::fs::canonicalize(path).map_err(|e| at(e.to_string()))?;
        if self.stack.contains(&canonical) {
            return Err(at("include cycle".into()));
        }
        if !self.seen.insert(canonical.clone()) {
            return Err(at("included more than once".into()));
        }
        let text = std::fs::read_to_string(&canonical).map_err(|e| at(e.to_string()))?;
        let mut table = parse(path, &text)?;
        if included && table.contains_key("project") {
            return Err(at(
                "'project' belongs in the project's own brokkr.toml".into()
            ));
        }
        if table.contains_key("results_server") {
            return Err(at("'results_server' belongs to the user config".into()));
        }
        let includes = match table.remove("include") {
            None => Vec::new(),
            Some(toml::Value::Array(items)) => items
                .into_iter()
                .map(|item| match item {
                    toml::Value::String(s) if !s.trim().is_empty() => Ok(s),
                    _ => Err(at("'include' entries must be non-empty path strings".into())),
                })
                .collect::<Result<Vec<_>, _>>()?,
            Some(_) => return Err(at("'include' must be an array of paths".into())),
        };
        let parent = canonical
            .parent()
            .ok_or_else(|| at("config has no parent".into()))?;
        self.stack.push(canonical.clone());
        for name in includes {
            self.visit(&parent.join(name), true, merged)?;
        }
        self.stack.pop();
        merge(merged, table, &mut Vec::new());
        Ok(())
    }
}

// Same merge rules as brokkr/config_parts/compose.rs, including the two
// atomic settings and named array shadowing.
fn merge(base: &mut toml::Table, overlay: toml::Table, path: &mut Vec<String>) {
    if matches!(path.as_slice(), [table, _] if table == "bench")
        && ["script", "script_file"]
            .iter()
            .any(|key| overlay.contains_key(*key))
    {
        base.remove("script");
        base.remove("script_file");
    }
    for (key, value) in overlay {
        path.push(key.clone());
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(b)), toml::Value::Table(o)) if !matches!(path.as_slice(), [table, _] if table == "cargo_env") =>
            {
                merge(b, o, path);
            }
            (Some(toml::Value::Array(b)), toml::Value::Array(o))
                if table_array(b)
                    && table_array(&o)
                    && !matches!(path.as_slice(), [table, _, key] if table == "bench" && key == "changelog") =>
            {
                let names: HashSet<String> =
                    o.iter().filter_map(entry_name).map(str::to_owned).collect();
                b.retain(|item| entry_name(item).is_none_or(|name| !names.contains(name)));
                b.extend(o);
            }
            (_, value) => {
                base.insert(key, value);
            }
        }
        path.pop();
    }
}

fn table_array(items: &[toml::Value]) -> bool {
    !items.is_empty() && items.iter().all(toml::Value::is_table)
}

fn entry_name(item: &toml::Value) -> Option<&str> {
    item.get("name").and_then(toml::Value::as_str)
}
