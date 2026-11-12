//! `--describe-harness`: print the describe document, every scenario
//! checked against the protocol's rules first. Writes nothing anywhere
//! else.

use crate::runner::Exit;

/// The describe document, or why a declaration breaks the protocol.
pub(crate) fn document() -> Result<serde_json::Value, String> {
    let mut scenarios = Vec::new();
    for scenario in crate::scenarios::all() {
        let workload = scenario.workload();
        workload.validate()?;
        let decl = crate::scenarios::declared(&workload);
        decl.validate()?;
        scenarios.push(decl.to_json());
    }
    Ok(serde_json::json!({ "protocol": crate::protocol::PROTOCOL, "scenarios": scenarios }))
}

pub(crate) fn run() -> Exit {
    let Ok(doc) = document() else {
        return Exit::Refused;
    };
    let Ok(text) = serde_json::to_string(&doc) else {
        return Exit::Refused;
    };
    if crate::describe_out::write_document(&text) {
        Exit::Passed
    } else {
        Exit::Refused
    }
}

#[cfg(test)]
mod tests {
    /// Every declared scenario passes the protocol's rules and its
    /// workload's own, and the plan names exactly the declared calls.
    #[test]
    fn the_describe_document_is_valid() {
        let doc = super::document().expect("valid declarations");
        assert_eq!(doc["protocol"], 1);
        for scenario in crate::scenarios::all() {
            let workload = scenario.workload();
            let decl = crate::scenarios::declared(&workload);
            let plan = workload.plan().expect("plan");
            let planned: Vec<&String> = plan["calls"].as_object().expect("calls").keys().collect();
            let mut declared: Vec<&str> = decl.calls.iter().map(|c| c.id).collect();
            declared.sort_unstable();
            assert_eq!(planned, declared, "{}", decl.id);
        }
    }

    /// Required capabilities no scenario covers yet, each with the ticket
    /// that owns its proof: `brokkr library --all` classifies them
    /// unsupported and does not certify until a scenario covers them.
    const UNCOVERED: &[(&str, &str)] = &[];

    /// The ids of brokkr.toml's `[pbfhogg.library.require]` table.
    ///
    /// This is a convention-bound reader, not a TOML parser: the table must
    /// be one `[pbfhogg.library.require]` table in the root brokkr.toml, one
    /// `"id" = "meaning"` line per entry. It refuses rather than guesses on
    /// anything outside that convention: a table line of another shape, a
    /// second mention of the table, any `library` key in an included file
    /// and any nested include. The certifying comparison is brokkr's own,
    /// made by `brokkr library --all` against the composed configuration;
    /// this test keeps the declarations and the table in step before that.
    fn required() -> std::collections::BTreeSet<String> {
        const HEADER: &str = "[pbfhogg.library.require]";
        let root = pbfhogg_test_scratch::repository_root();
        let text = std::fs::read_to_string(root.join("brokkr.toml")).expect("brokkr.toml");
        let mentions = code_lines(&text)
            .filter(|l| {
                // The key, not the line: an entry's meaning may say "require".
                let key = leading_key(l);
                key == "pbfhogg" || key.split('.').any(|part| part == "require")
            })
            .count();
        assert_eq!(
            mentions, 1,
            "brokkr.toml must name {HEADER} exactly once, as its header"
        );
        let mut ids = std::collections::BTreeSet::new();
        for line in code_lines(&text)
            .skip_while(|l| l.trim() != HEADER)
            .skip(1)
            .take_while(|l| !l.trim_start().starts_with('['))
        {
            let entry = line
                .trim()
                .strip_prefix('"')
                .and_then(|rest| rest.split_once("\" = \""))
                .filter(|(id, meaning)| !id.contains(['"', '\\']) && meaning.ends_with('"'));
            let Some((id, _)) = entry else {
                panic!("{HEADER} line outside the one-quoted-key convention: {line}");
            };
            assert!(ids.insert(id.to_owned()), "{id} is required twice");
        }
        // Includes are listed one quoted path per line in the root's
        // `include = [ ... ]` array; none may carry library keys or include
        // further files, since this reader composes nothing.
        for line in code_lines(&text)
            .skip_while(|l| leading_key(l) != "include")
            .skip(1)
            .take_while(|l| l.trim() != "]")
        {
            let include = line.trim().trim_end_matches(',').trim_matches('"');
            let body = std::fs::read_to_string(root.join(include))
                .unwrap_or_else(|e| panic!("include {include}: {e}"));
            for line in code_lines(&body) {
                assert!(
                    !refused_in_include(&leading_key(line)),
                    "{include} carries a library key or an include, which this reader cannot compose: {line}"
                );
            }
        }
        ids
    }

    /// Lines that are neither blank nor comments.
    fn code_lines(text: &str) -> impl Iterator<Item = &str> {
        text.lines().filter(|line| {
            let line = line.trim();
            !line.is_empty() && !line.starts_with('#')
        })
    }

    /// The key at the start of a line, header brackets and quotes dropped:
    /// `[pbfhogg.library]`, `pbfhogg.library.x = 1` and `include = [` give
    /// `pbfhogg.library`, `pbfhogg.library.x` and `include`.
    fn leading_key(line: &str) -> String {
        line.trim_start()
            .trim_start_matches('[')
            .chars()
            .take_while(|c| !matches!(c, '=' | ']' | ' ' | '\t'))
            .filter(|&c| c != '"')
            .collect()
    }

    /// A key that could add to the require table, or compose further files.
    fn refused_in_include(key: &str) -> bool {
        let names = |root: &str| {
            key == root
                || key
                    .strip_prefix(root)
                    .is_some_and(|rest| rest.starts_with('.'))
        };
        names("pbfhogg") || names("library") || key == "include"
    }

    #[test]
    fn include_reader_refuses_what_it_cannot_compose() {
        for line in [
            "[pbfhogg.library.require]",
            "[pbfhogg]",
            "pbfhogg.library.require.\"x\" = \"y\"",
            "\"pbfhogg\".library = {}",
            "library.require = {}",
            "include = [",
        ] {
            assert!(refused_in_include(&leading_key(line)), "{line}");
        }
        for line in [
            "include_ignored = true",
            "    \"pbfhogg-cli\",",
            "[bench.library-x]",
        ] {
            assert!(!refused_in_include(&leading_key(line)), "{line}");
        }
    }

    /// The declarations and the require table move together: every id a
    /// scenario covers is required, every required id is covered or listed
    /// in [`UNCOVERED`] (and then covered by nothing), and every call
    /// claims its stream.
    #[test]
    fn declarations_match_the_require_table() {
        let required = required();
        assert!(!required.is_empty(), "no [pbfhogg.library.require] ids");
        let mut covered = std::collections::BTreeSet::new();
        for scenario in crate::scenarios::all() {
            let decl = crate::scenarios::declared(&scenario.workload());
            for call in &decl.calls {
                assert_eq!(
                    call.stream,
                    crate::protocol::STREAM_SEALED,
                    "{}/{}",
                    decl.id,
                    call.id
                );
            }
            covered.extend(decl.capabilities().map(str::to_owned));
        }
        for id in &covered {
            assert!(required.contains(id), "{id} is covered but not required");
        }
        for id in &required {
            let gap = UNCOVERED.iter().any(|(g, _)| g == id);
            assert_eq!(
                covered.contains(id),
                !gap,
                "{id}: covered or listed uncovered"
            );
        }
        for (id, _) in UNCOVERED {
            assert!(
                required.contains(*id),
                "{id} is listed uncovered but not required"
            );
        }
    }
}
