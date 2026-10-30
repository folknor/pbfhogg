//! `--describe-harness`: print the describe document, every scenario
//! checked against the protocol's rules first. Writes nothing anywhere
//! else.

use crate::runner::Exit;

/// The describe document, or why a declaration breaks the protocol.
pub(crate) fn document() -> Result<serde_json::Value, String> {
    let mut scenarios = Vec::new();
    for scenario in crate::scenarios::all() {
        let decl = crate::scenarios::declared(scenario.as_ref());
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
    /// Every declared scenario passes the protocol's rules, and the plan
    /// names exactly the declared calls.
    #[test]
    fn the_describe_document_is_valid() {
        let doc = super::document().expect("valid declarations");
        assert_eq!(doc["protocol"], 1);
        for scenario in crate::scenarios::all() {
            let decl = crate::scenarios::declared(scenario.as_ref());
            let plan = scenario.plan();
            let planned: Vec<&String> = plan["calls"].as_object().expect("calls").keys().collect();
            let mut declared: Vec<&str> = decl.calls.iter().map(|c| c.id).collect();
            declared.sort_unstable();
            assert_eq!(planned, declared, "{}", decl.id);
        }
    }

    /// Required capabilities no scenario covers yet, each with the ticket
    /// that owns its proof: `brokkr library --all` classifies them
    /// unsupported and does not certify until a scenario covers them.
    const UNCOVERED: &[(&str, &str)] = &[
        ("executor.caller-pool.v1", "pbfhogg#34"),
        ("host.page-cache-keep.v1", "pbfhogg#39"),
        ("counters.own-snapshot.v1", "pbfhogg#39"),
    ];

    /// The ids of brokkr.toml's `[pbfhogg.library.require]` table: one
    /// quoted key per line, up to the next table header.
    fn required() -> std::collections::BTreeSet<String> {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../brokkr.toml");
        let text = std::fs::read_to_string(path).expect("brokkr.toml");
        text.lines()
            .skip_while(|l| l.trim() != "[pbfhogg.library.require]")
            .skip(1)
            .take_while(|l| !l.trim_start().starts_with('['))
            .filter_map(|l| l.trim().strip_prefix('"')?.split_once('"'))
            .map(|(id, _)| id.to_owned())
            .collect()
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
            let decl = crate::scenarios::declared(scenario.as_ref());
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
