//! The documents of brokkr's library protocol, version 1, as this harness
//! writes them, and the protocol's limits on their text (`brokkr man
//! library`, Documents and The describe document). Every number is written
//! as a JSON integer.

use serde_json::{Value, json};

/// The protocol version every document carries.
pub(crate) const PROTOCOL: u64 = 1;

/// The longest one-line text (a description, a label).
const ONE_LINE_MAX: usize = 200;
/// The longest outcome message.
pub(crate) const MESSAGE_MAX: usize = 1000;
/// The longest refusal or seal reason.
pub(crate) const REASON_MAX: usize = 1000;
/// The longest diagnostic message and result detail.
pub(crate) const DETAIL_MAX: usize = 4000;
/// The longest diagnostic key.
pub(crate) const KEY_MAX: usize = 200;

/// A scenario, call, role or assertion id: `[a-z0-9][a-z0-9_.-]{0,63}`.
pub(crate) fn is_identifier(id: &str) -> bool {
    let bytes = id.as_bytes();
    let first_ok = bytes
        .first()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    first_ok
        && bytes.len() <= 64
        && bytes.iter().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-')
        })
}

/// One-line text: 1 to 200 bytes, no control character.
pub(crate) fn is_one_line(text: &str) -> bool {
    !text.is_empty() && text.len() <= ONE_LINE_MAX && !text.chars().any(char::is_control)
}

/// `text` cut to at most `max` bytes at a character boundary, and whether
/// anything was cut.
pub(crate) fn cut(text: &str, max: usize) -> (&str, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

/// `text` as one line of at most `max` bytes: every control character
/// replaced by a space, then cut at a character boundary.
pub(crate) fn one_line(text: &str, max: usize) -> String {
    let line: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    cut(&line, max).0.to_owned()
}

/// The kind of a generated role this harness writes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum GeneratedKind {
    File,
    Pbf,
}

/// What a role is: where its path comes from, and its kind.
#[derive(Clone, Copy)]
pub(crate) enum Role {
    /// A dataset file of the run, read only, in the named variant.
    Dataset { variant: &'static str },
    /// A file setup writes before any call, read by calls.
    Generated { kind: GeneratedKind },
    /// A PBF output written by its owner call; a seeded one starts as its
    /// seed (`brokkr-library-seed-v1 <role>` and a newline).
    OutputPbf { owner: &'static str, seeded: bool },
}

/// The bytes a seeded output role starts as.
pub(crate) fn seed(role: &str) -> Vec<u8> {
    format!("brokkr-library-seed-v1 {role}\n").into_bytes()
}

pub(crate) struct RoleDecl {
    pub(crate) id: &'static str,
    pub(crate) role: Role,
}

impl RoleDecl {
    pub(crate) const fn dataset(id: &'static str, variant: &'static str) -> Self {
        Self {
            id,
            role: Role::Dataset { variant },
        }
    }

    pub(crate) const fn output(id: &'static str, owner: &'static str) -> Self {
        Self {
            id,
            role: Role::OutputPbf {
                owner,
                seeded: false,
            },
        }
    }

    fn to_json(&self) -> Value {
        match self.role {
            Role::Dataset { variant } => json!({
                "id": self.id, "kind": "pbf", "source": "dataset", "variant": variant,
            }),
            Role::Generated { kind } => json!({
                "id": self.id,
                "kind": if kind == GeneratedKind::Pbf { "pbf" } else { "file" },
                "source": "generated",
            }),
            Role::OutputPbf { owner, seeded } => {
                let mut value = json!({
                    "id": self.id, "kind": "pbf", "source": "output", "owner": owner,
                });
                if let (true, Some(object)) = (seeded, value.as_object_mut()) {
                    object.insert("seeded".to_owned(), json!(true));
                }
                value
            }
        }
    }
}

/// One argument of a CLI counterpart.
#[derive(Clone, Copy)]
pub(crate) enum Arg {
    Literal(&'static str),
    Input(&'static str),
    Output(&'static str),
}

/// What a counterpart's replay must come to.
#[derive(Clone, Copy)]
pub(crate) enum Expect {
    /// Exit 0 with outputs equal to the call's.
    Output,
    /// Refuse with this exit status, publishing nothing, as the call did.
    Refusal { exit: u8 },
}

/// A call's CLI counterpart.
pub(crate) struct Counterpart {
    pub(crate) argv: &'static [Arg],
    /// The command writes run scratch (`--tmp-dir` negotiated).
    pub(crate) scratch: bool,
    pub(crate) expect: Expect,
    /// `Some(stance)` declares the overwrite stance; `None` leaves it to
    /// brokkr's negotiation.
    pub(crate) overwrite: Option<bool>,
    pub(crate) policy_differences: &'static [&'static str],
}

impl Counterpart {
    /// A counterpart that must write equal outputs, without scratch.
    pub(crate) const fn output(
        argv: &'static [Arg],
        overwrite: Option<bool>,
        policy_differences: &'static [&'static str],
    ) -> Self {
        Self {
            argv,
            scratch: false,
            expect: Expect::Output,
            overwrite,
            policy_differences,
        }
    }

    fn to_json(&self) -> Value {
        let argv: Vec<Value> = self
            .argv
            .iter()
            .map(|arg| match *arg {
                Arg::Literal(text) => json!(text),
                Arg::Input(role) => json!({ "input": role }),
                Arg::Output(role) => json!({ "output": role }),
            })
            .collect();
        let mut cli = json!({
            "argv": argv,
            "scratch": self.scratch,
            "policy_differences": self.policy_differences,
        });
        if let Some(object) = cli.as_object_mut() {
            match self.expect {
                Expect::Output => {
                    object.insert("expect".to_owned(), json!("output"));
                }
                Expect::Refusal { exit } => {
                    object.insert("expect".to_owned(), json!("refusal"));
                    object.insert("exit".to_owned(), json!(exit));
                }
            }
            if let Some(stance) = self.overwrite {
                object.insert("overwrite".to_owned(), json!(stance));
            }
        }
        cli
    }
}

pub(crate) struct CallDecl {
    pub(crate) id: &'static str,
    pub(crate) label: &'static str,
    pub(crate) cli: Option<Counterpart>,
}

pub(crate) struct AssertionDecl {
    pub(crate) id: &'static str,
    /// The call it is about, or `None` for the scenario as a whole.
    pub(crate) call: Option<&'static str>,
}

/// A scenario's entry in the describe document.
pub(crate) struct ScenarioDecl {
    pub(crate) id: &'static str,
    pub(crate) description: &'static str,
    pub(crate) deadline_s: u64,
    /// One call, no other library work in the process: profilable.
    pub(crate) hotpath: bool,
    /// One call timed against its CLI counterpart (brokkr#58).
    pub(crate) isolation: bool,
    pub(crate) calls: Vec<CallDecl>,
    pub(crate) roles: Vec<RoleDecl>,
    pub(crate) assertions: Vec<AssertionDecl>,
}

impl ScenarioDecl {
    pub(crate) fn to_json(&self) -> Value {
        let calls: Vec<Value> = self
            .calls
            .iter()
            .map(|call| {
                let mut value = json!({ "id": call.id, "label": call.label });
                if let (Some(cli), Some(object)) = (&call.cli, value.as_object_mut()) {
                    object.insert("cli".to_owned(), cli.to_json());
                }
                value
            })
            .collect();
        let roles: Vec<Value> = self.roles.iter().map(RoleDecl::to_json).collect();
        let assertions: Vec<Value> = self
            .assertions
            .iter()
            .map(|a| json!({ "id": a.id, "call": a.call }))
            .collect();
        let mut value = json!({
            "id": self.id,
            "description": self.description,
            "deadline_s": self.deadline_s,
            "calls": calls,
            "roles": roles,
            "assertions": assertions,
        });
        if let Some(object) = value.as_object_mut() {
            if self.hotpath {
                object.insert("hotpath".to_owned(), json!(true));
            }
            if self.isolation {
                object.insert("isolation".to_owned(), json!(true));
            }
        }
        value
    }

    /// The protocol's rules on a declaration, checked before the harness
    /// prints or runs it: identifiers, one-line text, unique ids, owners
    /// and placeholders naming roles of the right source.
    pub(crate) fn validate(&self) -> Result<(), String> {
        let ok_id = |what: &str, id: &str| {
            if is_identifier(id) {
                Ok(())
            } else {
                Err(format!("{}: {what} {id:?} is not an identifier", self.id))
            }
        };
        ok_id("scenario", self.id)?;
        if !is_one_line(self.description) || !(1..=86_400).contains(&self.deadline_s) {
            return Err(format!("{}: description or deadline out of range", self.id));
        }
        if self.calls.is_empty() {
            return Err(format!("{}: no calls", self.id));
        }
        unique(self.calls.iter().map(|c| c.id), "call", self.id)?;
        unique(self.roles.iter().map(|r| r.id), "role", self.id)?;
        unique(self.assertions.iter().map(|a| a.id), "assertion", self.id)?;
        for call in &self.calls {
            ok_id("call", call.id)?;
            if call.id == "setup" || !is_one_line(call.label) {
                return Err(format!(
                    "{}: call {} is setup or its label is bad",
                    self.id, call.id
                ));
            }
            if let Some(cli) = &call.cli {
                self.validate_counterpart(call.id, cli)?;
            }
        }
        let generated = self
            .roles
            .iter()
            .any(|r| matches!(r.role, Role::Generated { .. }));
        if (self.hotpath || self.isolation) && (self.calls.len() != 1 || generated) {
            return Err(format!(
                "{}: a hotpath or isolation scenario has one call and no generated role",
                self.id
            ));
        }
        if self.isolation
            && !self
                .calls
                .iter()
                .all(|c| matches!(&c.cli, Some(cli) if matches!(cli.expect, Expect::Output)))
        {
            return Err(format!(
                "{}: an isolation call needs an output-expecting counterpart",
                self.id
            ));
        }
        for role in &self.roles {
            ok_id("role", role.id)?;
            if let Role::OutputPbf { owner, .. } = role.role
                && !self.calls.iter().any(|c| c.id == owner)
            {
                return Err(format!("{}: role {} has no owner call", self.id, role.id));
            }
        }
        for assertion in &self.assertions {
            ok_id("assertion", assertion.id)?;
            let call_ok = assertion
                .call
                .is_none_or(|call| self.calls.iter().any(|c| c.id == call));
            if assertion.id.starts_with("brokkr.") || !call_ok {
                return Err(format!("{}: assertion {} is bad", self.id, assertion.id));
            }
        }
        Ok(())
    }

    fn validate_counterpart(&self, call: &str, cli: &Counterpart) -> Result<(), String> {
        let bad = |why: &str| Err(format!("{}: counterpart of {call}: {why}", self.id));
        let Some(Arg::Literal(first)) = cli.argv.first() else {
            return bad("the first token is not the subcommand");
        };
        if cli.argv.len() < 2 || !is_identifier(first) {
            return bad("argv too short or bad subcommand");
        }
        let mut outputs = Vec::new();
        for arg in cli.argv {
            match *arg {
                Arg::Literal(text) => {
                    let refused = ["--overwrite", "-O", "--staging-dir", "--tmp-dir"]
                        .iter()
                        .any(|flag| text == *flag || text.starts_with(&format!("{flag}=")));
                    if text.is_empty()
                        || text.contains('/')
                        || [".", "..", "--"].contains(&text)
                        || refused
                    {
                        return bad("a literal names a path or a negotiated flag");
                    }
                }
                Arg::Input(role) => {
                    if !self.has_role(role, |r| {
                        matches!(r, Role::Dataset { .. } | Role::Generated { .. })
                    }) {
                        return bad("an input placeholder names no input role");
                    }
                }
                Arg::Output(role) => {
                    if outputs.contains(&role)
                        || !self.has_role(
                            role,
                            |r| matches!(r, Role::OutputPbf { owner, .. } if owner == call),
                        )
                    {
                        return bad("an output placeholder is repeated or not this call's");
                    }
                    outputs.push(role);
                }
            }
        }
        if outputs.is_empty() {
            return bad("no output placeholder");
        }
        if let Expect::Refusal { exit } = cli.expect
            && (!(2..=127).contains(&exit) || exit == 64)
        {
            return bad("a refusal's exit is outside 2 to 127 or is 64");
        }
        if !cli.policy_differences.iter().all(|t| is_one_line(t)) {
            return bad("a policy difference is not one line");
        }
        Ok(())
    }

    fn has_role(&self, id: &str, pred: impl Fn(Role) -> bool) -> bool {
        self.roles.iter().any(|r| r.id == id && pred(r.role))
    }

    /// The output roles `call` owns.
    pub(crate) fn outputs_of(&self, call: &str) -> Vec<&'static str> {
        self.roles
            .iter()
            .filter(|r| matches!(r.role, Role::OutputPbf { owner, .. } if owner == call))
            .map(|r| r.id)
            .collect()
    }
}

fn unique<'a>(
    ids: impl Iterator<Item = &'a str>,
    what: &str,
    scenario: &str,
) -> Result<(), String> {
    let mut seen = std::collections::BTreeSet::new();
    for id in ids {
        if !seen.insert(id) {
            return Err(format!("{scenario}: duplicate {what} id {id}"));
        }
    }
    Ok(())
}

/// A result's status.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Status {
    Pass,
    Fail,
}

/// One reported result.
pub(crate) struct Outcome {
    pub(crate) id: &'static str,
    pub(crate) status: Status,
    pub(crate) detail: String,
}

impl Outcome {
    /// `pass` when `ok`, else `fail`, with `detail` either way.
    pub(crate) fn judge(id: &'static str, ok: bool, detail: impl Into<String>) -> Self {
        Self {
            id,
            status: if ok { Status::Pass } else { Status::Fail },
            detail: detail.into(),
        }
    }

    pub(crate) fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "status": match self.status { Status::Pass => "pass", Status::Fail => "fail" },
            "detail": cut(&self.detail, DETAIL_MAX).0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{cut, is_identifier, is_one_line, one_line};

    #[test]
    fn identifiers_follow_the_protocol_alphabet() {
        for good in ["a", "0", "repeat-cat", "a.outcome", &"a".repeat(64)] {
            assert!(is_identifier(good), "{good:?}");
        }
        for bad in ["", "-a", "_a", "A", "a b", "a/b", &"a".repeat(65)] {
            assert!(!is_identifier(bad), "{bad:?}");
        }
    }

    #[test]
    fn one_line_text_is_bounded_and_control_free() {
        assert!(is_one_line("cat input -> out-a"));
        assert!(!is_one_line(""));
        assert!(!is_one_line("a\nb"));
        assert!(!is_one_line(&"x".repeat(201)));
    }

    #[test]
    fn cuts_land_on_character_boundaries() {
        assert_eq!(cut("abc", 3), ("abc", false));
        assert_eq!(cut("a\u{e9}b", 2), ("a", true));
        assert_eq!(one_line("a\nb\tc", 10), "a b c");
        assert_eq!(one_line("\u{e9}\u{e9}", 3), "\u{e9}");
    }
}
