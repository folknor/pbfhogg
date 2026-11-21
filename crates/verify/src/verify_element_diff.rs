//! The element diff the classifying checks read: the verifier's merge of
//! osmium's decodings of two files ([`crate::opl_compare`]), streamed into
//! per-class counts with bounded ID samples.
//!
//! With the first PBF as the old side and the second as the new side, a
//! delete is an element only the first holds, a create one only the second
//! holds, and a modify a key both hold with unequal records. What each class
//! means is the caller's policy: extract gates content and per-strategy
//! membership.

use std::collections::BTreeSet;
use std::path::Path;

use crate::counts::Counts;
use crate::error::Error;
use crate::harness::{Harness, Role};
use crate::opl_compare::{self, Class, Policy};
use crate::osc::{OscAction, OscKind};

/// Cap on the IDs kept per kind per class, so a runaway difference does not
/// bury the summary. The counts stay exact.
pub(crate) use crate::tunables::MAX_REPORTED_IDS;

/// A count of entries and the smallest [`MAX_REPORTED_IDS`] distinct IDs
/// among them.
#[derive(Default)]
pub(crate) struct Sampled {
    pub(crate) count: u64,
    smallest: BTreeSet<i64>,
}

impl Sampled {
    pub(crate) fn add(&mut self, id: i64) {
        self.count += 1;
        self.smallest.insert(id);
        if self.smallest.len() > MAX_REPORTED_IDS {
            self.smallest.pop_last();
        }
    }

    #[cfg(test)]
    pub(crate) fn ids(&self) -> Vec<i64> {
        self.smallest.iter().copied().collect()
    }
}

/// One outcome class, per kind (node, way, relation).
#[derive(Default)]
pub(crate) struct PerType {
    pub(crate) kinds: [Sampled; 3],
}

impl PerType {
    pub(crate) fn add(&mut self, kind: OscKind, id: i64) {
        self.kinds[kind.index()].add(id);
    }

    pub(crate) fn total(&self) -> u64 {
        self.kinds.iter().map(|k| k.count).sum()
    }

    /// `"N nodes, N ways, N relations"`.
    pub(crate) fn counts_line(&self) -> String {
        let [n, w, r] = &self.kinds;
        format!("{} nodes, {} ways, {} relations", n.count, w.count, r.count)
    }

    /// Print `<prefix><label>: <counts>` and the ID samples of every
    /// non-empty kind, through `emit`; nothing when the class is empty.
    pub(crate) fn print(&self, prefix: &str, label: &str, mut emit: impl FnMut(&str)) {
        if self.total() == 0 {
            return;
        }
        emit(&format!("  {prefix}{label}: {}", self.counts_line()));
        let [n, w, r] = &self.kinds;
        print_id_sample("nodes", n, &mut emit);
        print_id_sample("ways", w, &mut emit);
        print_id_sample("relations", r, &mut emit);
    }
}

fn print_id_sample(kind: &str, sampled: &Sampled, emit: &mut impl FnMut(&str)) {
    if sampled.count == 0 {
        return;
    }
    let shown = sampled.smallest.len() as u64;
    let suffix = if sampled.count > shown {
        format!(" (smallest IDs; {} entries in all)", sampled.count)
    } else {
        String::new()
    };
    let list = sampled
        .smallest
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    emit(&format!("    {kind}: {list}{suffix}"));
}

/// Compare `old` with `new` as osmium decodes them under `policy`
/// ([`opl_compare`]), handing every differing identity to `each` in object
/// order, its listing at `listing`: [`OscAction::Delete`] for an object only
/// `old` holds, [`OscAction::Create`] for one only `new` holds and
/// [`OscAction::Modify`] for one both hold with unequal records.
///
/// Returns only once both streams have been read whole, so a caller acts on
/// what it accumulated only after `Ok`. A side out of strict kind and ID
/// order is an error, never a verdict.
pub(crate) fn scan_element_diff(
    harness: &mut Harness,
    old: &Path,
    new: &Path,
    listing: &Path,
    policy: Policy,
    each: impl FnMut(OscAction, OscKind, i64),
) -> Result<(), Error> {
    opl_compare::compare(harness, old, new, listing, policy, classify(each))?;
    Ok(())
}

/// [`scan_element_diff`] with each side's order judged under its [`Role`]
/// from the same decodings ([`Harness::judge_walk`]); returns both sides'
/// counts.
pub(crate) fn scan_element_diff_judged(
    harness: &mut Harness,
    old: (&Path, Role<'_>),
    new: (&Path, Role<'_>),
    listing: &Path,
    policy: Policy,
    each: impl FnMut(OscAction, OscKind, i64),
) -> Result<[Counts; 2], Error> {
    let walked =
        opl_compare::compare_judged(harness, old.0, new.0, listing, policy, classify(each))?;
    Ok(harness.judge_walk([old, new], walked)?.1)
}

fn classify(mut each: impl FnMut(OscAction, OscKind, i64)) -> impl FnMut(opl_compare::Group) {
    let kinds = [OscKind::Node, OscKind::Way, OscKind::Relation];
    move |group| {
        let action = match group.class {
            Class::FirstOnly => OscAction::Delete,
            Class::SecondOnly => OscAction::Create,
            Class::Differs => OscAction::Modify,
        };
        each(action, kinds[group.kind], group.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_huge_class_keeps_exact_counts_and_bounded_samples() {
        let mut class = PerType::default();
        for id in (0..100_000).rev() {
            class.add(OscKind::Node, id);
        }
        assert_eq!(class.kinds[0].count, 100_000);
        assert_eq!(class.kinds[0].ids(), (0..10).collect::<Vec<i64>>());
        assert_eq!(MAX_REPORTED_IDS, 10);
        assert_eq!(class.total(), 100_000);
    }
}
