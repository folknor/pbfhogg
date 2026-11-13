//! A call's publications held against its output roles on disk
//! after the supervisor proves quiescence: each role's state is
//! replayed from its initial state through the members that name it, in
//! ordinal order, and the state that leaves must be the one the file system
//! shows once the harness was released.
//!
//! The replay rests on two producer obligations: a call changes its output
//! roles only through its publications, and publications sharing a role run
//! serially in ordinal order, rollback included. It proves the destination
//! is in the state the effects describe, not which generation a published
//! one holds: that a restore after an earlier installation put back the
//! right generation is the harness's assertion to make.

use std::path::Path;

use super::artefact::{CallDoc, Effect};
use super::describe::{Role, Scenario};
use super::seed::{self, State};

/// What a role's destination holds by the effects reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expected {
    Absent,
    Seed,
    /// A generation some publication installed.
    Published,
}

impl Expected {
    fn name(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Seed => "its seed",
            Self::Published => "a published generation",
        }
    }
}

/// The state a role starts in: its seed when seeded, else absent.
pub(crate) fn initial(role: &Role) -> State {
    if role.seeded() {
        State::Seed
    } else {
        State::Absent
    }
}

/// The output roles `call` owns, in describe order.
pub(crate) fn outputs_of<'a>(
    scenario: &'a Scenario,
    call: &'a str,
) -> impl Iterator<Item = &'a Role> {
    scenario.roles.iter().filter(move |r| r.is_output_of(call))
}

/// Every way `call`'s reported effects disagree with its output roles under
/// `roles_dir`, each a failure line.
pub(crate) fn failures(
    scenario: &Scenario,
    call: &str,
    doc: &CallDoc,
    roles_dir: &Path,
) -> Vec<String> {
    let mut failures = Vec::new();
    for role in outputs_of(scenario, call) {
        let mut expected = match initial(role) {
            State::Seed => Expected::Seed,
            State::Absent | State::Other => Expected::Absent,
        };
        let mut defect = None;
        for (i, publication) in doc.publications.iter().enumerate() {
            let Some((_, effect)) = publication.members.iter().find(|(r, _)| *r == role.id) else {
                continue;
            };
            match effect {
                Effect::None => {}
                Effect::Installed => expected = Expected::Published,
                Effect::Restored | Effect::Displaced if expected == Expected::Absent => {
                    defect = Some(format!(
                        "call {call}: publication {} reports {} {}, which had no destination \
                         to move aside",
                        i + 1,
                        effect.name(),
                        role.id
                    ));
                    break;
                }
                Effect::Restored => {}
                Effect::Displaced => expected = Expected::Absent,
            }
        }
        if let Some(defect) = defect {
            failures.push(defect);
            continue;
        }
        let found = match seed::state(&roles_dir.join(&role.id), role.kind, &role.id) {
            Ok(found) => found,
            Err(e) => {
                failures.push(format!("call {call}: output {}: {e}", role.id));
                continue;
            }
        };
        let agrees = matches!(
            (expected, found),
            (Expected::Absent, State::Absent)
                | (Expected::Seed, State::Seed)
                | (Expected::Published, State::Other)
        );
        if !agrees {
            let why = if expected == Expected::Published && found == State::Seed {
                ", and a seed is never published"
            } else {
                ""
            };
            failures.push(format!(
                "call {call}: by its publications output {} holds {}, but it is {}{why}",
                role.id,
                expected.name(),
                found.name()
            ));
        }
    }
    failures
}

#[cfg(test)]
#[path = "publication_tests.rs"]
mod tests;
