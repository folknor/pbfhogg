//! Strict worker artefact reader for the independent verifier.
//!
//! Call only after the supervisor has proved quiescence with ECHILD.
//! Files are opened without following links or blocking on special files;
//! types and byte limits are checked on the open descriptor. Directories
//! are inspected without following links. This is protocol validation after
//! quiescence, not protection against concurrent mutation.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;
use std::path::Path;

use super::describe::{self, Scenario};
use super::json::{self, Field, Obj};

/// The largest artefact document accepted, in bytes.
pub(crate) const FILE_LIMIT: usize = 16 * 1024 * 1024;

/// The largest refusal document accepted, in bytes.
const REFUSAL_LIMIT: usize = 64 * 1024;

/// Longest result detail, in bytes.
const DETAIL_LIMIT: usize = 4000;

/// Longest error-outcome message, seal refusal or harness refusal reason.
const MESSAGE_LIMIT: usize = 1000;

/// Longest diagnostic key and message, in bytes.
const DIAGNOSTIC_KEY_LIMIT: usize = 200;
const DIAGNOSTIC_MESSAGE_LIMIT: usize = 4000;

/// Longest snapshot counter name, in bytes: short enough that its sealing
/// record fits the FIFO line budget whatever the call id and value.
const COUNTER_NAME_LIMIT: usize = 400;

/// The scenario-level document's name.
pub(crate) const SCENARIO_FILE: &str = "scenario.json";

/// The directory holding one document per call.
pub(crate) const CALLS_DIR: &str = "calls";

/// The document a harness that could not run (exit 2) may write.
pub(crate) const REFUSAL_FILE: &str = "refusal.json";

/// One reported result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Outcome {
    pub(crate) id: String,
    pub(crate) status: Status,
    pub(crate) detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    Pass,
    Fail,
    Unevaluated,
}

impl Status {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Unevaluated => "unevaluated",
        }
    }
}

/// How a call's library command returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CallOutcome {
    Ok,
    Cancelled,
    Error { kind: String, message: String },
}

/// What sealing the call's invocation came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Seal {
    Sealed,
    /// The census at the seal attempt: every value the caller still held,
    /// counted by producer-defined kind, each count at least 1.
    Outstanding(BTreeMap<String, u64>),
    Refused(String),
}

impl Seal {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Sealed => "sealed",
            Self::Outstanding(_) => "outstanding",
            Self::Refused(_) => "refused",
        }
    }
}

/// One captured `host::Diagnostics` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Diagnostic {
    /// `MarkerFifo::now_us()` when it was captured.
    pub(crate) at_us: u64,
    pub(crate) kind: String,
    pub(crate) key: String,
    pub(crate) message: String,
    pub(crate) truncated: bool,
}

/// One decided publication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Publication {
    pub(crate) decision: Decision,
    /// Each destination it covers, by output role, with what the attempt
    /// left there.
    pub(crate) members: Vec<(String, Effect)>,
}

/// A publication's decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    Authorized,
    RefusedCancelled,
    RefusedError,
}

/// What one publication attempt left at a member's destination when it
/// finished, relative to the destination just before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Effect {
    /// The destination is as it was (a rollback to absence included).
    None,
    /// The new generation is at the destination (an incomplete rollback
    /// included).
    Installed,
    /// The previous generation was moved aside and put back.
    Restored,
    /// The previous generation was moved aside and not put back: the
    /// destination is absent.
    Displaced,
}

impl Effect {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Installed => "installed",
            Self::Restored => "restored",
            Self::Displaced => "displaced",
        }
    }
}

/// How the call's sink ended, by its own account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SinkState {
    Sealed,
    Failed,
    Open,
}

impl SinkState {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Sealed => "sealed",
            Self::Failed => "failed",
            Self::Open => "open",
        }
    }
}

/// The artefact `stream` object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StreamAccount {
    pub(crate) state: SinkState,
    /// Every line the sink dropped, its terminal record included.
    pub(crate) drops: u64,
}

/// A validated call document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CallDoc {
    pub(crate) entry_us: u64,
    pub(crate) return_us: u64,
    pub(crate) seal_us: u64,
    pub(crate) outcome: CallOutcome,
    /// Every publication the call decided, in ordinal order.
    pub(crate) publications: Vec<Publication>,
    pub(crate) seal: Seal,
    /// The final counters; present exactly when the call sealed.
    pub(crate) snapshot: Option<BTreeMap<String, u64>>,
    pub(crate) stream: StreamAccount,
    pub(crate) diagnostics: Vec<Diagnostic>,
    pub(crate) results: Vec<Outcome>,
    /// The call's `context`, as validated (a nonempty plain object).
    pub(crate) context: json::Json,
}

/// A validated scenario document: its results, its `plan`, and the
/// diagnostics of setup and the harness's own work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScenarioDoc {
    pub(crate) results: Vec<Outcome>,
    pub(crate) plan: json::Json,
    pub(crate) diagnostics: Vec<Diagnostic>,
}

/// The documents of a well-formed artefact tree, unparsed.
pub(crate) struct Tree {
    pub(crate) scenario: Vec<u8>,
    /// One per declared call, in describe order.
    pub(crate) calls: Vec<(String, Vec<u8>)>,
}

/// Read `path`, a regular file of at most `limit` bytes, without following
/// a symlink or blocking on a special file.
pub(crate) fn read_regular(path: &Path, limit: usize) -> Result<Vec<u8>, String> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let name = path.display();
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| format!("{name}: cannot open: {e}"))?;
    let meta = file
        .metadata()
        .map_err(|e| format!("{name}: cannot inspect: {e}"))?;
    if !meta.is_file() {
        return Err(format!("{name}: not a regular file"));
    }
    if meta.len() > limit as u64 {
        return Err(format!(
            "{name}: {} bytes, over the {limit}-byte limit",
            meta.len()
        ));
    }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{name}: cannot read: {e}"))?;
    if bytes.len() > limit {
        return Err(format!(
            "{name}: grew past the {limit}-byte limit while read"
        ));
    }
    Ok(bytes)
}

/// The kind of the entry at `path`, never following a symlink; `None` when
/// absent.
pub(crate) fn entry_kind(path: &Path) -> Result<Option<std::fs::FileType>, String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => Ok(Some(meta.file_type())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: cannot inspect: {e}", path.display())),
    }
}

/// The names in directory `dir`, each with its own (unfollowed) type.
pub(crate) fn entries(dir: &Path) -> Result<Vec<(String, std::fs::FileType)>, String> {
    let read =
        std::fs::read_dir(dir).map_err(|e| format!("{}: cannot list: {e}", dir.display()))?;
    let mut out = Vec::new();
    for entry in read {
        let entry = entry.map_err(|e| format!("{}: cannot list: {e}", dir.display()))?;
        let kind = entry
            .file_type()
            .map_err(|e| format!("{}: cannot inspect: {e}", entry.path().display()))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|raw| format!("{}: an entry named {raw:?} is not UTF-8", dir.display()))?;
        out.push((name, kind));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// Check the tree's shape and read its documents: exactly `scenario.json`
/// and `calls/` holding exactly one `<call id>.json` per declared call.
pub(crate) fn read_tree(dir: &Path, scenario: &Scenario) -> Result<Tree, String> {
    match entry_kind(dir)? {
        Some(kind) if kind.is_dir() => {}
        Some(_) => return Err(format!("{}: not a directory", dir.display())),
        None => return Err(format!("{}: missing", dir.display())),
    }
    let mut seen_scenario = false;
    let mut seen_calls = false;
    for (name, kind) in entries(dir)? {
        match name.as_str() {
            SCENARIO_FILE if kind.is_file() => seen_scenario = true,
            CALLS_DIR if kind.is_dir() => seen_calls = true,
            SCENARIO_FILE => return Err(format!("{SCENARIO_FILE} is not a regular file")),
            CALLS_DIR => return Err(format!("{CALLS_DIR} is not a directory")),
            other => return Err(format!("an unexpected entry {other:?}")),
        }
    }
    if !seen_scenario {
        return Err(format!("{SCENARIO_FILE} is missing"));
    }
    if !seen_calls {
        return Err(format!("{CALLS_DIR}/ is missing"));
    }
    let calls_dir = dir.join(CALLS_DIR);
    let declared: BTreeSet<String> = scenario
        .calls
        .iter()
        .map(|c| format!("{}.json", c.id))
        .collect();
    for (name, kind) in entries(&calls_dir)? {
        if !declared.contains(&name) {
            return Err(format!(
                "{CALLS_DIR}/{name:?} is not a declared call's document"
            ));
        }
        if !kind.is_file() {
            return Err(format!("{CALLS_DIR}/{name} is not a regular file"));
        }
    }
    let mut calls = Vec::new();
    for call in &scenario.calls {
        let path = calls_dir.join(format!("{}.json", call.id));
        if entry_kind(&path)?.is_none() {
            return Err(format!("{CALLS_DIR}/{}.json is missing", call.id));
        }
        calls.push((call.id.clone(), read_regular(&path, FILE_LIMIT)?));
    }
    Ok(Tree {
        scenario: read_regular(&dir.join(SCENARIO_FILE), FILE_LIMIT)?,
        calls,
    })
}

/// The scenario document: its results and plan, after the plan's checks.
pub(crate) fn parse_scenario(
    bytes: &[u8],
    scenario: &Scenario,
    forbidden: &[&str],
) -> Result<ScenarioDoc, String> {
    let doc = json::parse(SCENARIO_FILE, bytes)?;
    let root = Field::root(&doc, SCENARIO_FILE);
    let top = root.as_object()?;
    describe::protocol(&top)?;
    same_id(&top, "scenario", &scenario.id)?;
    let plan = top.required("plan")?;
    plan.check_plain(forbidden)?;
    let plan_obj = plan.as_object()?;
    let calls = plan_obj.required("calls")?;
    let members = calls.as_members()?;
    let declared: BTreeSet<&str> = scenario.calls.iter().map(|c| c.id.as_str()).collect();
    for (key, value) in &members {
        if !declared.contains(key) {
            return Err(format!("{}: {key:?} is not a declared call", calls.path()));
        }
        if value.as_members()?.is_empty() {
            return Err(format!("{}: an empty object", value.path()));
        }
    }
    let given: BTreeSet<&str> = members.iter().map(|(k, _)| *k).collect();
    if let Some(missing) = declared.difference(&given).next() {
        return Err(format!("{}: call {missing:?} is missing", calls.path()));
    }
    let results = results(&top.required("assertions")?)?;
    let diagnostics = diagnostics(&top.required("diagnostics")?)?;
    top.finish()?;
    Ok(ScenarioDoc {
        results,
        plan: plan.json().clone(),
        diagnostics,
    })
}

/// One call document, `call` its declared id in `scenario`.
pub(crate) fn parse_call(
    bytes: &[u8],
    scenario: &Scenario,
    call: &str,
    forbidden: &[&str],
) -> Result<CallDoc, String> {
    let name = format!("{CALLS_DIR}/{call}.json");
    let doc = json::parse(&name, bytes)?;
    let root = Field::root(&doc, &name);
    let top = root.as_object()?;
    describe::protocol(&top)?;
    same_id(&top, "call", call)?;
    let time = |key: &str| -> Result<u64, String> {
        let field = top.required(key)?;
        let n = field.as_int_in(0, i128::from(i64::MAX))?;
        u64::try_from(n).map_err(|_| format!("{}: out of range", field.path()))
    };
    let (entry_us, return_us, seal_us) = (time("entry_us")?, time("return_us")?, time("seal_us")?);
    if !(entry_us <= return_us && return_us <= seal_us) {
        return Err(format!(
            "{name}: entry_us {entry_us}, return_us {return_us}, seal_us {seal_us} are not in order"
        ));
    }
    let outcome = outcome(&top.required("outcome")?)?;
    let publications = publications(&top.required("publications")?, scenario, call)?;
    cancel(&top.required("cancel")?, &publications)?;
    let seal = seal(&top.required("seal")?)?;
    let snapshot = snapshot(&top.required("snapshot")?, &seal)?;
    let stream_field = top.required("stream")?;
    let stream = stream(&stream_field)?;
    // A sink is terminalized only by a successful seal of its invocation.
    if (stream.state == SinkState::Open) != (seal != Seal::Sealed) {
        return Err(format!(
            "{}: a stream is open exactly when the call did not seal, but it is {} and the \
             seal is {}",
            stream_field.path(),
            stream.state.name(),
            seal.name()
        ));
    }
    let context = top.required("context")?;
    context.check_plain(forbidden)?;
    if context.as_members()?.is_empty() {
        return Err(format!("{}: an empty object", context.path()));
    }
    let diagnostics_field = top.required("diagnostics")?;
    let diagnostics = diagnostics(&diagnostics_field)?;
    // A call's diagnostics are captured from its entry on; setup's go in
    // scenario.json.
    if let Some((i, early)) = diagnostics
        .iter()
        .enumerate()
        .find(|(_, d)| d.at_us < entry_us)
    {
        return Err(format!(
            "{}[{i}].at_us: {} is before the call's entry_us {entry_us}",
            diagnostics_field.path(),
            early.at_us
        ));
    }
    let results = results(&top.required("assertions")?)?;
    top.finish()?;
    Ok(CallDoc {
        entry_us,
        return_us,
        seal_us,
        outcome,
        publications,
        seal,
        snapshot,
        stream,
        diagnostics,
        results,
        context: context.json().clone(),
    })
}

/// The reason in a harness's refusal document.
pub(crate) fn parse_refusal(bytes: &[u8]) -> Result<String, String> {
    let doc = json::parse(REFUSAL_FILE, bytes)?;
    let root = Field::root(&doc, REFUSAL_FILE);
    let top = root.as_object()?;
    describe::protocol(&top)?;
    let reason = describe::one_line_in(&top.required("reason")?, 1, MESSAGE_LIMIT)?;
    top.finish()?;
    Ok(reason)
}

/// Read a refusal document at `path`, the size limit applied.
pub(crate) fn read_refusal(path: &Path) -> Result<String, String> {
    parse_refusal(&read_regular(path, REFUSAL_LIMIT)?)
}

fn same_id(obj: &Obj<'_>, key: &str, expected: &str) -> Result<(), String> {
    let field = obj.required(key)?;
    let found = field.as_str()?;
    if found == expected {
        Ok(())
    } else {
        Err(format!("{}: {found:?}, not {expected:?}", field.path()))
    }
}

fn results(field: &Field<'_>) -> Result<Vec<Outcome>, String> {
    let mut out: Vec<Outcome> = Vec::new();
    for item in field.as_array()? {
        let obj = item.as_object()?;
        let id = describe::identifier(&obj.required("id")?)?;
        let status_field = obj.required("status")?;
        let status = match status_field.as_str()? {
            "pass" => Status::Pass,
            "fail" => Status::Fail,
            "unevaluated" => Status::Unevaluated,
            other => {
                return Err(format!(
                    "{}: unknown status {other:?} (expected pass, fail or unevaluated)",
                    status_field.path()
                ));
            }
        };
        let detail_field = obj.required("detail")?;
        let detail = detail_field.as_str()?;
        if detail.len() > DETAIL_LIMIT {
            return Err(format!(
                "{}: {} bytes, over the {DETAIL_LIMIT}-byte limit",
                detail_field.path(),
                detail.len()
            ));
        }
        obj.finish()?;
        if out.iter().any(|o| o.id == id) {
            return Err(format!("{}: result {id:?} is reported twice", item.path()));
        }
        out.push(Outcome {
            id,
            status,
            detail: detail.to_owned(),
        });
    }
    Ok(out)
}

fn outcome(field: &Field<'_>) -> Result<CallOutcome, String> {
    let obj = field.as_object()?;
    let status = obj.required("status")?;
    let outcome = match status.as_str()? {
        "ok" => CallOutcome::Ok,
        "cancelled" => CallOutcome::Cancelled,
        "error" => CallOutcome::Error {
            kind: describe::identifier(&obj.required("kind")?)?,
            message: describe::one_line_in(&obj.required("message")?, 0, MESSAGE_LIMIT)?,
        },
        other => {
            return Err(format!(
                "{}: unknown outcome {other:?} (expected ok, cancelled or error)",
                status.path()
            ));
        }
    };
    obj.finish()?;
    Ok(outcome)
}

/// The call's publications: ordinals 1, 2, ... without gaps, each with its
/// decision and members, every member an output role `call` owns, at most
/// once per publication, and every member of a refused one `none`.
fn publications(
    field: &Field<'_>,
    scenario: &Scenario,
    call: &str,
) -> Result<Vec<Publication>, String> {
    let mut out = Vec::new();
    for (i, item) in field.as_array()?.iter().enumerate() {
        let obj = item.as_object()?;
        let ordinal = obj.required("ordinal")?;
        let expected = i128::try_from(i + 1).unwrap_or(i128::MAX);
        if ordinal.as_int_in(1, i128::from(u64::MAX))? != expected {
            return Err(format!(
                "{}: ordinals run 1, 2, ... without gaps; expected {expected}",
                ordinal.path()
            ));
        }
        let decision_field = obj.required("decision")?;
        let decision = match decision_field.as_str()? {
            "authorized" => Decision::Authorized,
            "refused_cancelled" => Decision::RefusedCancelled,
            "refused_error" => Decision::RefusedError,
            other => {
                return Err(format!(
                    "{}: unknown decision {other:?} (expected authorized, refused_cancelled or \
                     refused_error)",
                    decision_field.path()
                ));
            }
        };
        let members_field = obj.required("members")?;
        let mut members: Vec<(String, Effect)> = Vec::new();
        for entry in members_field.as_array()? {
            let member = entry.as_object()?;
            let role_field = member.required("role")?;
            let role = describe::identifier(&role_field)?;
            if !scenario
                .roles
                .iter()
                .any(|r| r.id == role && r.is_output_of(call))
            {
                return Err(format!(
                    "{}: {role:?} is not an output role of call {call:?}",
                    role_field.path()
                ));
            }
            if members.iter().any(|(r, _)| *r == role) {
                return Err(format!(
                    "{}: role {role:?} is a member twice",
                    role_field.path()
                ));
            }
            let effect_field = member.required("effect")?;
            let effect = match effect_field.as_str()? {
                "none" => Effect::None,
                "installed" => Effect::Installed,
                "restored" => Effect::Restored,
                "displaced" => Effect::Displaced,
                other => {
                    return Err(format!(
                        "{}: unknown effect {other:?} (expected none, installed, restored or \
                         displaced)",
                        effect_field.path()
                    ));
                }
            };
            if effect != Effect::None && decision != Decision::Authorized {
                return Err(format!(
                    "{}: a refused publication leaves every member none",
                    effect_field.path()
                ));
            }
            member.finish()?;
            members.push((role, effect));
        }
        if members.is_empty() {
            return Err(format!(
                "{}: a publication has at least one member",
                members_field.path()
            ));
        }
        obj.finish()?;
        out.push(Publication { decision, members });
    }
    Ok(out)
}

fn cancel(field: &Field<'_>, publications: &[Publication]) -> Result<(), String> {
    if field.is_null() {
        return Ok(());
    }
    let obj = field.as_object()?;
    let result = obj.required("result")?;
    let (expected, name) = match result.as_str()? {
        "no_publication_pending" => {
            obj.finish()?;
            return Ok(());
        }
        "before_decision" => (Decision::RefusedCancelled, "a refused_cancelled"),
        "after_decision" => (Decision::Authorized, "an authorized"),
        other => {
            return Err(format!(
                "{}: unknown cancel result {other:?} (expected before_decision, after_decision \
                 or no_publication_pending)",
                result.path()
            ));
        }
    };
    let ordinal_field = obj.required("ordinal")?;
    let ordinal =
        usize::try_from(ordinal_field.as_int_in(1, i128::from(u64::MAX))?).unwrap_or(usize::MAX);
    obj.finish()?;
    match publications.get(ordinal - 1) {
        Some(publication) if publication.decision == expected => {}
        _ => {
            return Err(format!(
                "{}: names publication {ordinal}, which is not {name} publication",
                ordinal_field.path()
            ));
        }
    }
    if let Some(later) = publications
        .iter()
        .skip(ordinal)
        .position(|p| p.decision == Decision::Authorized)
    {
        return Err(format!(
            "{}: publication {} was authorized after the cancel decided publication {ordinal}",
            field.path(),
            ordinal + later + 1
        ));
    }
    Ok(())
}

fn seal(field: &Field<'_>) -> Result<Seal, String> {
    let obj = field.as_object()?;
    let state = obj.required("state")?;
    let seal = match state.as_str()? {
        "sealed" => {
            obj.forbid("held", "on a sealed seal: nothing was held")?;
            Seal::Sealed
        }
        "outstanding" => {
            let held_field = obj.required("held")?;
            let mut held = BTreeMap::new();
            for (kind, count) in held_field.as_members()? {
                if !describe::is_identifier(kind) {
                    return Err(format!(
                        "{}: {kind:?} is not an identifier ([a-z0-9][a-z0-9_.-], at most 64 \
                         bytes)",
                        count.path()
                    ));
                }
                let n = count.as_int_in(1, i128::from(u64::MAX))?;
                held.insert(kind.to_owned(), u64::try_from(n).unwrap_or(u64::MAX));
            }
            if held.is_empty() {
                return Err(format!(
                    "{}: an outstanding seal holds at least one value",
                    held_field.path()
                ));
            }
            Seal::Outstanding(held)
        }
        "refused" => {
            obj.forbid("held", "on a refused seal")?;
            Seal::Refused(describe::one_line_in(
                &obj.required("reason")?,
                1,
                MESSAGE_LIMIT,
            )?)
        }
        other => {
            return Err(format!(
                "{}: unknown seal state {other:?} (expected sealed, outstanding or refused)",
                state.path()
            ));
        }
    };
    obj.finish()?;
    Ok(seal)
}

fn snapshot(field: &Field<'_>, seal: &Seal) -> Result<Option<BTreeMap<String, u64>>, String> {
    if *seal != Seal::Sealed {
        return if field.is_null() {
            Ok(None)
        } else {
            Err(format!(
                "{}: must be null unless the call sealed",
                field.path()
            ))
        };
    }
    let mut out = BTreeMap::new();
    for (name, value) in field.as_members()? {
        if name.is_empty()
            || name.len() > COUNTER_NAME_LIMIT
            || name.contains('=')
            || name.contains('\n')
        {
            return Err(format!(
                "{}: not a counter name (1 to {COUNTER_NAME_LIMIT} bytes, no `=`, no newline)",
                value.path()
            ));
        }
        let n = value.as_int_in(0, i128::from(u64::MAX))?;
        out.insert(name.to_owned(), u64::try_from(n).unwrap_or(u64::MAX));
    }
    Ok(Some(out))
}

fn stream(field: &Field<'_>) -> Result<StreamAccount, String> {
    let obj = field.as_object()?;
    let state_field = obj.required("state")?;
    let state = match state_field.as_str()? {
        "sealed" => SinkState::Sealed,
        "failed" => SinkState::Failed,
        "open" => SinkState::Open,
        other => {
            return Err(format!(
                "{}: unknown stream state {other:?} (expected sealed, failed or open)",
                state_field.path()
            ));
        }
    };
    let drops = obj.required("drops")?.as_int_in(0, i128::from(u64::MAX))?;
    obj.finish()?;
    Ok(StreamAccount {
        state,
        drops: u64::try_from(drops).unwrap_or(u64::MAX),
    })
}

fn diagnostics(field: &Field<'_>) -> Result<Vec<Diagnostic>, String> {
    let mut out = Vec::new();
    for item in field.as_array()? {
        let obj = item.as_object()?;
        let at = obj.required("at_us")?.as_int_in(0, i128::from(i64::MAX))?;
        let at_us = u64::try_from(at).unwrap_or(u64::MAX);
        let kind = describe::identifier(&obj.required("kind")?)?;
        let bounded = |key: &str, min: usize, max: usize| -> Result<String, String> {
            let f = obj.required(key)?;
            let text = f.as_str()?;
            if text.len() < min || text.len() > max {
                return Err(format!(
                    "{}: must be {min} to {max} bytes, is {}",
                    f.path(),
                    text.len()
                ));
            }
            Ok(text.to_owned())
        };
        let key = bounded("key", 1, DIAGNOSTIC_KEY_LIMIT)?;
        let message = bounded("message", 0, DIAGNOSTIC_MESSAGE_LIMIT)?;
        let truncated = obj.required("truncated")?.as_bool()?;
        obj.finish()?;
        out.push(Diagnostic {
            at_us,
            kind,
            key,
            message,
            truncated,
        });
    }
    Ok(out)
}

/// Every way the reported results differ from the declared inventory, each
/// naming the result: exactly one result per declared assertion, in the
/// document its `call` names.
pub(crate) fn inventory_failures(
    scenario: &Scenario,
    scenario_results: &[Outcome],
    call_results: &[(String, Vec<Outcome>)],
) -> Vec<String> {
    let mut failures = Vec::new();
    let mut check = |scope: Option<&str>, results: &[Outcome], name: &str| {
        let declared: BTreeSet<&str> = scenario
            .assertions
            .iter()
            .filter(|a| a.call.as_deref() == scope)
            .map(|a| a.id.as_str())
            .collect();
        for result in results {
            if !declared.contains(result.id.as_str()) {
                let elsewhere = scenario.assertions.iter().find(|a| a.id == result.id);
                failures.push(match elsewhere {
                    Some(a) => format!(
                        "{name} reports {:?}, which belongs to {}",
                        result.id,
                        a.call.as_deref().map_or_else(
                            || SCENARIO_FILE.to_owned(),
                            |c| format!("{CALLS_DIR}/{c}.json")
                        )
                    ),
                    None => format!(
                        "{name} reports {:?}, which describe does not declare",
                        result.id
                    ),
                });
            }
        }
        for id in declared {
            if !results.iter().any(|r| r.id == id) {
                failures.push(format!("{name} has no result for {id:?}"));
            }
        }
    };
    check(None, scenario_results, SCENARIO_FILE);
    for (call, results) in call_results {
        check(Some(call), results, &format!("{CALLS_DIR}/{call}.json"));
    }
    failures
}

#[cfg(test)]
#[path = "artefact_tests.rs"]
mod tests;
