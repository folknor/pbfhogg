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
}
