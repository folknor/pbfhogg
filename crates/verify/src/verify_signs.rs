//! Sign domain: osmium commands that select by ID fold a negative ID or
//! reference onto its absolute value (libosmium's `positive_id` and
//! `positive_ref`), and pbfhogg selects by the signed one (pbfhogg's
//! DEVIATIONS.md). Where the fold can change osmium's answer, the two
//! outputs differ with pbfhogg right, a comparison that cannot be made and
//! never a pbfhogg failure. [`refuse_folds`] reads one osmium output for
//! the witnesses of such a fold and refuses the comparison when it finds one.

use std::path::Path;

use crate::error::Error;
use crate::harness::Harness;
use crate::opl_compare::Policy;
use crate::osc::OscKind;
use crate::pbf_frames::Origin;

/// The derived-summary kind of [`NegativeSigns`], versioned with its meaning.
const NEGATIVE_SIGNS_KIND: &str = "negative-signs 1";

const KINDS: [&str; 3] = ["node", "way", "relation"];

/// The first negative object ID and the first negative way node ref or
/// relation member in a decoding, each in file order: a stored reading of
/// one osmium output.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
struct NegativeSigns {
    /// `(kind index, ID)`.
    id: Option<(usize, i64)>,
    /// `(holder kind index, holder ID, referenced kind index, ref)`.
    reference: Option<(usize, i64, usize, i64)>,
}

/// Which witnesses refuse: a negative reference always, a negative object
/// ID too when the check does not refuse an input holding one first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Witnesses {
    References,
    IdsAndReferences,
}

/// Refuse the comparison when osmium's output at `osmium_out` holds a
/// witness of a fold: under [`Witnesses::IdsAndReferences`] an object with
/// a negative ID, and always a way referencing a negative node or a relation
/// with a negative member. `tool` names the osmium command in the refusal.
///
/// Why the output suffices: osmium's ID sets are unions, so a fold only adds
/// records. A record added because its opposite-sign twin was matched or
/// referenced, or a matched or referenced record whose twin is added, puts a
/// negative object in the output; a fold through a negative ref or member is
/// made only from a record osmium writes with that ref intact (a dangling
/// `n-2` reaches `n2`, and the way holding it is written). So every record of
/// the output is read, shared ones too, not only the differing ones. The
/// reading is stored under the output's decoding key, so a warm rerun
/// neither decodes nor scans; `policy` is the one the comparison reads the
/// output with, so the decoding is shared.
pub(crate) fn refuse_folds(
    harness: &mut Harness,
    osmium_out: &Path,
    policy: Policy,
    label: &str,
    tool: &str,
    witnesses: Witnesses,
) -> Result<(), Error> {
    let key = crate::opl_compare::planned_keys(harness, &[osmium_out], policy)?
        .and_then(|keys| keys.into_iter().next())
        .map(|key| key.derived(NEGATIVE_SIGNS_KIND));
    let stored = key
        .as_ref()
        .and_then(|key| harness.summary_of::<NegativeSigns>(key));
    let reading = if let Some(stored) = stored {
        harness.run.revalidate()?;
        harness.checkpoint()?;
        stored
    } else {
        crate::opl_compare::decoded(harness, (osmium_out, Origin::Other), policy, |h, opl| {
            let mut id = None;
            let mut reference = None;
            crate::verify_opl::scan_objects(opl, |o| {
                if id.is_none() && o.id < 0 {
                    id = Some((o.kind.index(), o.id));
                }
                if reference.is_none() {
                    reference = o
                        .nodes
                        .iter()
                        .map(|&r| (OscKind::Node, r))
                        .chain(o.members.iter().copied())
                        .find(|&(_, r)| r < 0)
                        .map(|(kind, r)| (o.kind.index(), o.id, kind.index(), r));
                }
            })?;
            let reading = NegativeSigns { id, reference };
            // Stored only under the key this decoding was sealed under,
            // after the scan read it to its end.
            if let Some(key) = crate::opl_compare::decoding_key(h, opl) {
                h.publish_summary_of(&key.derived(NEGATIVE_SIGNS_KIND), &reading)?;
            }
            Ok(reading)
        })?
    };
    if let Some((holder_kind, holder, kind, id)) = reading.reference {
        return Err(Error::Tool(format!(
            "{label}: {} {holder} references {} {id}; {tool} matches a reference by its \
             absolute value and pbfhogg does not, so they cannot be compared on it",
            KINDS[holder_kind], KINDS[kind]
        )));
    }
    if witnesses == Witnesses::IdsAndReferences
        && let Some((kind, id)) = reading.id
    {
        return Err(Error::Tool(format!(
            "{label}: the output holds {} {id}; {tool} matches an ID by its absolute value and \
             pbfhogg does not, so they cannot be compared on it",
            KINDS[kind]
        )));
    }
    Ok(())
}
