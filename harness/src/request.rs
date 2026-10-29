//! The request brokkr writes for one launch (`brokkr man library`, Running
//! a scenario): `{"protocol": 1, "scenario": ID, "iteration": N,
//! "bindings": {ROLE: PATH}}`, nothing more and nothing less.

use crate::json::Json;
use std::collections::BTreeMap;
use std::path::PathBuf;

pub(crate) struct Request {
    pub(crate) scenario: String,
    /// Which process of the run this is. Read and checked, never used: it
    /// must not reach a plan or a context.
    #[allow(dead_code, reason = "validated, deliberately unused")]
    pub(crate) iteration: u64,
    pub(crate) bindings: BTreeMap<String, PathBuf>,
}

/// Read a request from its text.
pub(crate) fn parse(text: &str) -> Result<Request, String> {
    let Json::Obj(members) = crate::json::parse(text)? else {
        return Err("the request is not an object".into());
    };
    let mut protocol = None;
    let mut scenario = None;
    let mut iteration = None;
    let mut bindings = None;
    for (key, value) in members {
        match (key.as_str(), value) {
            ("protocol", Json::Int(n)) => protocol = Some(n),
            ("scenario", Json::Str(s)) => scenario = Some(s),
            ("iteration", Json::Int(n)) => iteration = Some(n),
            ("bindings", Json::Obj(b)) => bindings = Some(b),
            (key, _) => return Err(format!("request.{key}: unknown key or wrong type")),
        }
    }
    let (Some(protocol), Some(scenario), Some(iteration), Some(bindings)) =
        (protocol, scenario, iteration, bindings)
    else {
        return Err("the request lacks a required key".into());
    };
    if protocol != i128::from(crate::protocol::PROTOCOL) {
        return Err(format!(
            "request.protocol: {protocol}, this harness implements 1"
        ));
    }
    let iteration = u64::try_from(iteration)
        .ok()
        .filter(|n| i64::try_from(*n).is_ok())
        .ok_or("request.iteration: out of range")?;
    let mut paths = BTreeMap::new();
    for (role, path) in bindings {
        let Json::Str(path) = path else {
            return Err(format!("request.bindings.{role}: not a string"));
        };
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(format!("request.bindings.{role}: not absolute"));
        }
        paths.insert(role, path);
    }
    Ok(Request {
        scenario,
        iteration,
        bindings: paths,
    })
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn reads_a_request() {
        let request = parse(
            r#"{"protocol": 1, "scenario": "repeat-cat", "iteration": 3,
                "bindings": {"input": "/abs/in.pbf"}}"#,
        )
        .expect("valid");
        assert_eq!(request.scenario, "repeat-cat");
        assert_eq!(request.iteration, 3);
        assert_eq!(
            request.bindings["input"],
            std::path::Path::new("/abs/in.pbf")
        );
    }

    #[test]
    fn refuses_a_malformed_request() {
        for bad in [
            r#"{"protocol": 2, "scenario": "s", "iteration": 0, "bindings": {}}"#,
            r#"{"protocol": 1, "scenario": "s", "iteration": -1, "bindings": {}}"#,
            r#"{"protocol": 1, "scenario": "s", "iteration": 0}"#,
            r#"{"protocol": 1, "scenario": "s", "iteration": 0, "bindings": {}, "x": 1}"#,
            r#"{"protocol": 1, "scenario": "s", "iteration": 0, "bindings": {"a": "rel"}}"#,
            r#"{"protocol": 1, "scenario": 5, "iteration": 0, "bindings": {}}"#,
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }
}
