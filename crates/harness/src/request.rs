//! The request the library driver writes for one launch (`reference/library-protocol.md`, Running
//! a scenario): `{"protocol": 1, "scenario": ID, "iteration": N,
//! "bindings": {ROLE: PATH}}`, nothing more and nothing less. Every schema
//! refusal names its path into the request, as `request.iteration` or
//! `request.bindings["out-b"]`.

use crate::json::{Json, shown_key};
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

const KEYS: [&str; 4] = ["protocol", "scenario", "iteration", "bindings"];

/// Read a request from its bytes.
pub(crate) fn parse(bytes: &[u8]) -> Result<Request, String> {
    let root = crate::json::parse(bytes)?;
    let members = object(&root, "request")?;
    for (key, _) in members {
        if !KEYS.contains(&key.as_str()) {
            return Err(format!("request: unknown key {}", shown_key(key)));
        }
    }
    let field = |key: &str| {
        members
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, value)| value)
            .ok_or_else(|| format!("request: missing key {key:?}"))
    };
    let protocol = int(field("protocol")?, "request.protocol")?;
    if protocol != i128::from(crate::protocol::PROTOCOL) {
        return Err(format!(
            "request.protocol: {protocol}, this harness implements 1"
        ));
    }
    let Json::Str(scenario) = field("scenario")? else {
        return Err(wrong(field("scenario")?, "request.scenario", "a string"));
    };
    let iteration = int(field("iteration")?, "request.iteration")?;
    let iteration = u64::try_from(iteration)
        .ok()
        .filter(|n| i64::try_from(*n).is_ok())
        .ok_or("request.iteration: out of range")?;
    let mut paths = BTreeMap::new();
    for (role, path) in object(field("bindings")?, "request.bindings")? {
        let at = format!("request.bindings[{}]", shown_key(role));
        let Json::Str(path) = path else {
            return Err(wrong(path, &at, "a string"));
        };
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(format!("{at}: not absolute"));
        }
        paths.insert(role.clone(), path);
    }
    Ok(Request {
        scenario: scenario.clone(),
        iteration,
        bindings: paths,
    })
}

fn wrong(value: &Json, path: &str, expected: &str) -> String {
    format!("{path}: expected {expected}, found {}", value.kind())
}

/// The members of an object, refusing a duplicate decoded key.
fn object<'a>(value: &'a Json, path: &str) -> Result<&'a [(String, Json)], String> {
    let Json::Obj(members) = value else {
        return Err(wrong(value, path, "an object"));
    };
    let mut seen = std::collections::BTreeSet::new();
    for (key, _) in members {
        if !seen.insert(key.as_str()) {
            return Err(format!("{path}: duplicate key {}", shown_key(key)));
        }
    }
    Ok(members)
}

fn int(value: &Json, path: &str) -> Result<i128, String> {
    match value {
        Json::Int(n) => Ok(*n),
        _ => Err(wrong(value, path, "an integer")),
    }
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn reads_a_request() {
        let request = parse(
            br#"{"protocol": 1, "scenario": "repeat-cat", "iteration": 3,
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
    fn iteration_accepts_both_protocol_boundaries() {
        for iteration in [0, i64::MAX] {
            let text = format!(
                r#"{{"protocol":1,"scenario":"repeat-cat","iteration":{iteration},"bindings":{{}}}}"#
            );
            let request = parse(text.as_bytes()).expect("valid iteration boundary");
            assert_eq!(
                request.iteration,
                u64::try_from(iteration).expect("nonnegative")
            );
        }
    }

    #[test]
    fn every_schema_refusal_names_its_path() {
        let body = |rest: &str| format!(r#"{{"protocol": 1, "scenario": "s", {rest}}}"#);
        for (bad, expected) in [
            (
                r#"{"protocol": 2, "scenario": "s", "iteration": 0, "bindings": {}}"#.to_owned(),
                "request.protocol: 2",
            ),
            (
                body(r#""iteration": -1, "bindings": {}"#),
                "request.iteration: out of range",
            ),
            (
                body(r#""iteration": 0"#),
                r#"request: missing key "bindings""#,
            ),
            (
                body(r#""iteration": 0, "bindings": {}, "x": 1"#),
                r#"request: unknown key "x""#,
            ),
            (
                body(r#""iteration": 0, "bindings": {"a": "rel"}"#),
                r#"request.bindings["a"]: not absolute"#,
            ),
            (
                body(r#""iteration": 0, "bindings": {"a": 0}"#),
                r#"request.bindings["a"]: expected a string, found an integer"#,
            ),
            (
                r#"{"protocol": 1, "scenario": 5, "iteration": 0, "bindings": {}}"#.to_owned(),
                "request.scenario: expected a string, found an integer",
            ),
            (
                body(r#""iteration": -0, "bindings": {}"#),
                "request.iteration: expected an integer",
            ),
            (
                body(r#""iteration": 0.5, "bindings": {}"#),
                "request.iteration: expected an integer",
            ),
            (
                body(r#""iteration": 0, "iteration": 0, "bindings": {}"#),
                r#"request: duplicate key "iteration""#,
            ),
            (
                body(r#""iteration": 0, "bindings": {"a": "/x", "a": "/y"}"#),
                r#"request.bindings: duplicate key "a""#,
            ),
            (
                "[]".to_owned(),
                "request: expected an object, found an array",
            ),
            (
                body(r#""iteration": 0, "bindings": {}, "scenarix": 0"#),
                r#"request: unknown key "scenarix""#,
            ),
        ] {
            let refused = parse(bad.as_bytes()).err().expect("refused");
            assert!(refused.starts_with(expected), "{bad}: {refused}");
        }
    }
}
