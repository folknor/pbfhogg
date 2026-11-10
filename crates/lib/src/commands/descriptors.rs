//! Up-front descriptor admission for the commands whose descriptor count
//! grows with their arguments (`cat`'s inputs, multi-region `extract`'s
//! outputs): count what the run will hold, ask the host once
//! (`ProcessActions::fd_allowance`), and refuse the run before it acquires
//! any input when the answer is short, naming the counts and the remedy.
//! The library never raises the open-file limit itself (ADR-0016).

use crate::error::{Error, Result};

/// What one command's admission asks for and how it names it.
pub(crate) struct Demand<'a> {
    /// Descriptors the run opens and holds that grow with its arguments.
    pub(crate) held: usize,
    /// The command's fixed allowance on top of `held`, a tunable.
    pub(crate) extra: u64,
    /// What is held, for the refusal ("cat holds ...: 1100 input files").
    pub(crate) what: &'a str,
    /// The argument-side remedy, last in the refusal ("cat fewer files per
    /// call").
    pub(crate) fewer: &'a str,
}

/// Ask the host for `held + extra` descriptors and refuse the run up front
/// when it allows fewer. The caller holds the returned reservation until
/// the descriptors it was sized for are closed; a refusal drops a short
/// grant at once, so a reserving provider has it back.
pub(crate) fn admit(
    demand: &Demand<'_>,
    host: &crate::host::Entered<'_>,
) -> Result<crate::process::FdReservation> {
    let what = demand.what;
    let wanted = u64::try_from(demand.held)
        .ok()
        .and_then(|n| n.checked_add(demand.extra))
        .ok_or_else(|| {
            Error::invalid_argument(format!("{what}: more descriptors than a run can hold"))
        })?;
    let reservation = host.process.fd_allowance(wanted);
    let allowance = reservation.allowance();
    if allowance.descriptors < wanted {
        drop(reservation);
        return Err(Error::invalid_argument(format!(
            "{what} need {wanted} descriptors, but only {} are allowed ({allowance}); {}",
            allowance.descriptors,
            remedy(&allowance, wanted, demand.fewer)
        )));
    }
    Ok(reservation)
}

/// What a refused run's caller can do. A host grant is the host's policy,
/// which no process limit changes; a probed allowance is the process's soft
/// limit, which the caller (never the library) may raise up to the hard
/// limit when that is known to leave room. `fewer` is the command's own
/// way to ask for less.
fn remedy(allowance: &crate::process::FdAllowance, wanted: u64, fewer: &str) -> String {
    let Some(probe) = allowance.probe else {
        return format!(
            "grant more descriptors from the host's ProcessActions::fd_allowance, or {fewer}"
        );
    };
    let already_open = probe.soft_limit.saturating_sub(allowance.descriptors);
    let needed_limit = already_open.saturating_add(wanted);
    let raise = match probe.hard_limit {
        Some(hard) if hard >= needed_limit && hard > probe.soft_limit => format!(
            "raise the process's soft open-file limit to at least {needed_limit} (ulimit -n, \
             or setrlimit in an embedding program; the hard limit {hard} allows it)"
        ),
        Some(hard) if hard < needed_limit => format!(
            "the run needs an open-file limit of at least {needed_limit}, above the hard \
             limit {hard}; raise the hard limit"
        ),
        _ => "raise the process's soft open-file limit (ulimit -n, or setrlimit in an \
              embedding program)"
            .to_owned(),
    };
    format!("{raise}, close other descriptors, or {fewer}")
}
