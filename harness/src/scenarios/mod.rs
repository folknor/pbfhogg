//! The scenarios the harness declares, one file per family.

mod repeat_cat;

use crate::protocol::{AssertionDecl, Outcome, ScenarioDecl};
use crate::runner::Ctx;

/// The scenario-level assertion every scenario carries: no call diagnostic
/// arrived after its call document was written.
pub(crate) const DIAGNOSTICS_CLOSED: &str = "call-diagnostics-closed";

/// One scenario: its declaration, its plan, and its run.
pub(crate) trait Scenario {
    /// Its describe entry, without the assertions every scenario carries.
    fn decl(&self) -> ScenarioDecl;
    /// Every workload choice, resolved: nothing run-specific.
    fn plan(&self) -> serde_json::Value;
    /// Run the calls, writing each call document through `ctx`, and return
    /// the scenario's own results (all but [`DIAGNOSTICS_CLOSED`]). An
    /// `Err` refuses the run.
    fn run(&self, ctx: &mut Ctx<'_>) -> Result<Vec<Outcome>, String>;
}

/// Every scenario, in describe order.
pub(crate) fn all() -> Vec<Box<dyn Scenario>> {
    vec![Box::new(repeat_cat::RepeatCat)]
}

/// The scenario named `id`.
pub(crate) fn find(id: &str) -> Option<Box<dyn Scenario>> {
    all().into_iter().find(|s| s.decl().id == id)
}

/// A scenario's full declaration, the shared assertions included.
pub(crate) fn declared(scenario: &dyn Scenario) -> ScenarioDecl {
    let mut decl = scenario.decl();
    decl.assertions.push(AssertionDecl {
        id: DIAGNOSTICS_CLOSED,
        call: None,
    });
    decl
}
