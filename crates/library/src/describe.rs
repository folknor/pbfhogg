//! The describe document: the scenarios a library harness declares.
//!
//! `HARNESS --describe-harness` prints it; the verifier parses it strictly
//! ([`super::json`]) before anything is listed or run. The producer is
//! `crates/harness/src/describe.rs`; the reader shares none of its types.

use std::collections::BTreeSet;

use super::json::{self, Field, Obj};

/// The one protocol version this verifier implements.
pub(crate) const PROTOCOL: i128 = 1;

/// The largest describe document accepted, in bytes.
pub(crate) const DESCRIBE_LIMIT: usize = 1024 * 1024;

/// Longest one-line text (a description or a call label), in bytes.
const TEXT_LIMIT: usize = 200;

/// Longest scenario deadline, in seconds.
const DEADLINE_LIMIT_S: i128 = 86_400;

/// The call id naming the harness's own preparation, never a call.
const SETUP: &str = "setup";

/// The assertion-id prefix of the results the driver adds itself.
const BROKKR_PREFIX: &str = "brokkr.";

/// A validated describe document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Describe {
    pub(crate) scenarios: Vec<Scenario>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Scenario {
    pub(crate) id: String,
    pub(crate) description: String,
    pub(crate) deadline_s: u32,
    /// The scenario may be profiled (`--hotpath`, `--alloc`): it has exactly
    /// one call, and no other library work runs in its process, so the
    /// process-wide profile's functions are that call's.
    pub(crate) hotpath: bool,
    pub(crate) roles: Vec<Role>,
    pub(crate) calls: Vec<Call>,
    pub(crate) assertions: Vec<Assertion>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RoleKind {
    Pbf,
    File,
    Directory,
}

impl RoleKind {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Pbf => "pbf",
            Self::File => "file",
            Self::Directory => "directory",
        }
    }
}

/// Where a role's path comes from, and who writes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RoleSource {
    /// A dataset file of the run, read-only and shareable.
    Dataset { variant: String },
    /// Written by the harness's setup before any call, read by calls.
    Generated,
    /// Written by its one owner call. A `seeded` one starts as its seed
    /// (the seed protocol), written by the driver before the launch, so a call
    /// meets an existing destination.
    Output { owner: String, seeded: bool },
    /// A directory its one owner call uses as run scratch.
    Scratch { owner: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Role {
    pub(crate) id: String,
    pub(crate) kind: RoleKind,
    pub(crate) source: RoleSource,
}

impl Role {
    /// An output role `call` owns.
    pub(crate) fn is_output_of(&self, call: &str) -> bool {
        matches!(&self.source, RoleSource::Output { owner, .. } if owner == call)
    }

    pub(crate) fn seeded(&self) -> bool {
        matches!(self.source, RoleSource::Output { seeded: true, .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Call {
    pub(crate) id: String,
    pub(crate) label: String,
    /// The capability ids the call's stream verdict stands for; nonempty
    /// makes the driver add `<call>/brokkr.stream`.
    pub(crate) stream: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Assertion {
    pub(crate) id: String,
    /// The call it judges, or `None` for a scenario-level assertion.
    pub(crate) call: Option<String>,
    /// The capability ids its result stands for in a certification.
    pub(crate) covers: Vec<String>,
}

/// Parse and validate a describe document.
pub(crate) fn parse(bytes: &[u8]) -> Result<Describe, String> {
    if bytes.len() > DESCRIBE_LIMIT {
        return Err(format!(
            "the describe document is {} bytes, over the {DESCRIBE_LIMIT}-byte limit",
            bytes.len()
        ));
    }
    let doc = json::parse("the describe document", bytes)?;
    let root = Field::root(&doc, "describe");
    let top = root.as_object()?;
    protocol(&top)?;
    let mut scenarios = Vec::new();
    let mut ids = BTreeSet::new();
    let list = top.required("scenarios")?;
    for item in list.as_array()? {
        let scenario = scenario(&item)?;
        if !ids.insert(scenario.id.clone()) {
            return Err(format!(
                "{}: scenario id {:?} is declared twice",
                item.path(),
                scenario.id
            ));
        }
        scenarios.push(scenario);
    }
    if scenarios.is_empty() {
        return Err(format!("{}: declares no scenario", list.path()));
    }
    top.finish()?;
    Ok(Describe { scenarios })
}

/// Refuse any protocol version but the one this verifier implements.
pub(super) fn protocol(obj: &Obj<'_>) -> Result<(), String> {
    let field = obj.required("protocol")?;
    let version = field.as_int_in(i128::from(i64::MIN), i128::from(u64::MAX))?;
    if version == PROTOCOL {
        Ok(())
    } else {
        Err(format!(
            "{}: protocol {version}, but this verifier implements protocol {PROTOCOL} only",
            field.path()
        ))
    }
}

fn scenario(item: &Field<'_>) -> Result<Scenario, String> {
    let obj = item.as_object()?;
    let id = identifier(&obj.required("id")?)?;
    let description = one_line(&obj.required("description")?)?;
    let deadline = obj.required("deadline_s")?;
    let deadline_s = u32::try_from(deadline.as_int_in(1, DEADLINE_LIMIT_S)?)
        .map_err(|_| format!("{}: out of range", deadline.path()))?;

    let calls_field = obj.required("calls")?;
    let mut calls: Vec<Call> = Vec::new();
    for entry in calls_field.as_array()? {
        let call = call(&entry)?;
        if calls.iter().any(|c| c.id == call.id) {
            return Err(format!(
                "{}: call id {:?} is declared twice",
                entry.path(),
                call.id
            ));
        }
        calls.push(call);
    }
    if calls.is_empty() {
        return Err(format!("{}: declares no call", calls_field.path()));
    }
    let call_ids: BTreeSet<&str> = calls.iter().map(|c| c.id.as_str()).collect();

    let mut roles: Vec<Role> = Vec::new();
    for entry in obj.required("roles")?.as_array()? {
        let role = role(&entry, &call_ids)?;
        if roles.iter().any(|r| r.id == role.id) {
            return Err(format!(
                "{}: role id {:?} is declared twice",
                entry.path(),
                role.id
            ));
        }
        roles.push(role);
    }
    let mut assertions: Vec<Assertion> = Vec::new();
    for entry in obj.required("assertions")?.as_array()? {
        let assertion = assertion(&entry, &call_ids)?;
        if assertions.iter().any(|a| a.id == assertion.id) {
            return Err(format!(
                "{}: assertion id {:?} is declared twice",
                entry.path(),
                assertion.id
            ));
        }
        assertions.push(assertion);
    }
    let flag = |key: &str| -> Result<(bool, String), String> {
        match obj.optional(key) {
            Some(field) => Ok((field.as_bool()?, field.path().to_owned())),
            None => Ok((false, String::new())),
        }
    };
    let (hotpath, hotpath_path) = flag("hotpath")?;
    if hotpath && calls.len() != 1 {
        return Err(format!(
            "{hotpath_path}: a hotpath scenario has exactly one call (the profile is the \
             process's), this one has {}",
            calls.len()
        ));
    }
    obj.finish()?;
    Ok(Scenario {
        id,
        description,
        deadline_s,
        hotpath,
        roles,
        calls,
        assertions,
    })
}

fn call(item: &Field<'_>) -> Result<Call, String> {
    let obj = item.as_object()?;
    let id_field = obj.required("id")?;
    let id = identifier(&id_field)?;
    if id == SETUP {
        return Err(format!(
            "{}: `{SETUP}` names the harness's own preparation and is never a call id",
            id_field.path()
        ));
    }
    let label = one_line(&obj.required("label")?)?;
    let stream = capabilities(&obj, "stream")?;
    obj.finish()?;
    Ok(Call { id, label, stream })
}

/// A capability id: an identifier ending `.v<N>`, N a positive decimal
/// without a leading zero, so a capability whose meaning changes gets a new
/// id.
pub(crate) fn is_capability(id: &str) -> bool {
    is_identifier(id)
        && id.rsplit_once(".v").is_some_and(|(base, n)| {
            !base.is_empty()
                && !n.is_empty()
                && !n.starts_with('0')
                && n.bytes().all(|b| b.is_ascii_digit())
        })
}

fn capability(field: &Field<'_>) -> Result<String, String> {
    let id = field.as_str()?;
    if is_capability(id) {
        Ok(id.to_owned())
    } else {
        Err(format!(
            "{}: {id:?} is not a capability id (an identifier ending .v<N>, such as \
             cancel.pre-decision.v1)",
            field.path()
        ))
    }
}

/// The optional list of capability ids under `key`, each at most once;
/// absent is empty.
fn capabilities(obj: &Obj<'_>, key: &str) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(list) = obj.optional(key) {
        for item in list.as_array()? {
            let capability = capability(&item)?;
            if out.contains(&capability) {
                return Err(format!(
                    "{}: capability {capability:?} is listed twice",
                    item.path()
                ));
            }
            out.push(capability);
        }
    }
    Ok(out)
}

fn role(item: &Field<'_>, calls: &BTreeSet<&str>) -> Result<Role, String> {
    let obj = item.as_object()?;
    let id = identifier(&obj.required("id")?)?;
    let kind_field = obj.required("kind")?;
    let kind = match kind_field.as_str()? {
        "pbf" => RoleKind::Pbf,
        "file" => RoleKind::File,
        "directory" => RoleKind::Directory,
        other => {
            return Err(format!(
                "{}: unknown role kind {other:?} (expected pbf, file or directory)",
                kind_field.path()
            ));
        }
    };
    let source_field = obj.required("source")?;
    let source_name = source_field.as_str()?;
    let allowed: &[RoleKind] = match source_name {
        "dataset" => &[RoleKind::Pbf],
        "generated" | "output" => &[RoleKind::Pbf, RoleKind::File, RoleKind::Directory],
        "scratch" => &[RoleKind::Directory],
        other => {
            return Err(format!(
                "{}: unknown role source {other:?} (expected dataset, generated, output or \
                 scratch)",
                source_field.path()
            ));
        }
    };
    if !allowed.contains(&kind) {
        return Err(format!(
            "{}: a role of source {source_name} cannot be of kind {}",
            kind_field.path(),
            kind.name()
        ));
    }
    let shape = format!("on a role of source {source_name}");
    if source_name != "output" {
        obj.forbid(
            "seeded",
            &format!("{shape}: only an output role starts seeded"),
        )?;
    }
    let source = match source_name {
        "dataset" => {
            obj.forbid("owner", &format!("{shape}: a dataset role is read-only"))?;
            RoleSource::Dataset {
                variant: variant(&obj.required("variant")?)?,
            }
        }
        "generated" => {
            obj.forbid("owner", &format!("{shape}: setup owns it"))?;
            obj.forbid("variant", &shape)?;
            RoleSource::Generated
        }
        _ => {
            obj.forbid("variant", &shape)?;
            let owner = owner(&obj.required("owner")?, calls)?;
            if source_name == "output" {
                let seeded = match obj.optional("seeded") {
                    Some(f) => f.as_bool()?,
                    None => false,
                };
                RoleSource::Output { owner, seeded }
            } else {
                RoleSource::Scratch { owner }
            }
        }
    };
    obj.finish()?;
    Ok(Role { id, kind, source })
}

fn owner(field: &Field<'_>, calls: &BTreeSet<&str>) -> Result<String, String> {
    let owner = field.as_str()?;
    if calls.contains(owner) {
        Ok(owner.to_owned())
    } else {
        Err(format!(
            "{}: owner {owner:?} is not a call of this scenario",
            field.path()
        ))
    }
}

fn assertion(item: &Field<'_>, calls: &BTreeSet<&str>) -> Result<Assertion, String> {
    let obj = item.as_object()?;
    let id_field = obj.required("id")?;
    let id = identifier(&id_field)?;
    if id.starts_with(BROKKR_PREFIX) {
        return Err(format!(
            "{}: assertion ids starting `{BROKKR_PREFIX}` are reserved for the results the driver \
             adds",
            id_field.path()
        ));
    }
    let call_field = obj.required("call")?;
    let call = if call_field.is_null() {
        None
    } else {
        Some(owner(&call_field, calls)?)
    };
    let covers = capabilities(&obj, "covers")?;
    obj.finish()?;
    Ok(Assertion { id, call, covers })
}

/// `[a-z0-9][a-z0-9_.-]{0,63}`: valid as a FIFO call id, a file name and a
/// command-line word without escaping.
pub(super) fn is_identifier(id: &str) -> bool {
    let bytes = id.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes.iter().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-')
        })
}

pub(super) fn identifier(field: &Field<'_>) -> Result<String, String> {
    let id = field.as_str()?;
    if is_identifier(id) {
        Ok(id.to_owned())
    } else {
        Err(format!(
            "{}: {id:?} is not an identifier ([a-z0-9][a-z0-9_.-], at most 64 bytes)",
            field.path()
        ))
    }
}

/// A dataset variant name: grammar only here; the run's dataset resolves it.
fn variant(field: &Field<'_>) -> Result<String, String> {
    let name = field.as_str()?;
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
    if ok {
        Ok(name.to_owned())
    } else {
        Err(format!(
            "{}: {name:?} is not a variant name ([A-Za-z0-9_.-], 1 to 64 bytes)",
            field.path()
        ))
    }
}

fn one_line(field: &Field<'_>) -> Result<String, String> {
    one_line_in(field, 1, TEXT_LIMIT)
}

/// One-line text (no control character) of `min..=max` bytes.
pub(super) fn one_line_in(field: &Field<'_>, min: usize, max: usize) -> Result<String, String> {
    let text = field.as_str()?;
    if text.len() < min || text.len() > max {
        return Err(format!(
            "{}: must be {min} to {max} bytes, is {}",
            field.path(),
            text.len()
        ));
    }
    if text.chars().any(char::is_control) {
        return Err(format!(
            "{}: one line of text, without control characters",
            field.path()
        ));
    }
    Ok(text.to_owned())
}

#[cfg(test)]
#[path = "describe_tests.rs"]
mod tests;
