//! The blob layout `pbfhogg cat --dedupe` documents, predicted from the
//! inputs' framing as the verifier decodes it, and an output held to it.
//!
//! The rule (docs/cli/commands.md, cat `--dedupe`): blobs are ordered by
//! kind, then by first ID, ties in argument then file order; a run grows
//! while the next blob of its kind starts at or below the widest ID the run
//! reaches so far, so overlap is followed transitively; a blob in no run is
//! passed through with its serialized `Blob` payload unchanged, and a run's
//! blobs are merged and re-encoded in its place. The prediction covers only
//! blobs that rule describes alone: every blob nonempty, one kind, one
//! element field per group, strictly ascending non-negative IDs, and any
//! indexdata exactly its decoded kind, range and count. Anything else is
//! outside the domain (an empty blob is omitted, a mixed block is
//! restructured, a disordered one rewritten), a verifier error.

use std::path::Path;

use crate::error::Error;
use crate::pbf_frames::{self, ClaimedBlob};

/// One data blob of one input, as decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Entry {
    pub blob: ClaimedBlob,
    pub kind: usize,
    pub min: i64,
    pub max: i64,
}

/// Every data blob of `path`, decoded and held to the domain.
pub(crate) fn entries(path: &Path) -> Result<Vec<Entry>, Error> {
    let outside = |blob: &ClaimedBlob, why: String| {
        Error::tool(format!(
            "data blob {} of {} {why}, outside the layout this check predicts",
            blob.index,
            path.display()
        ))
    };
    let mut out = Vec::new();
    let mut claims = pbf_frames::Claims::open(path)?;
    while let Some(blob) = claims.next_blob()? {
        let (ids, single_field) =
            pbf_frames::blob_ids_and_shape(path, blob.payload_offset, blob.datasize)?;
        let Some(&(kind, min)) = ids.first() else {
            return Err(outside(&blob, "holds no element".into()));
        };
        if !single_field {
            return Err(outside(&blob, "has a group with two element fields".into()));
        }
        if ids.iter().any(|&(k, _)| k != kind) {
            return Err(outside(&blob, "holds more than one element kind".into()));
        }
        if min < 0 {
            return Err(outside(&blob, "holds a negative ID".into()));
        }
        if ids.windows(2).any(|w| w[0].1 >= w[1].1) {
            return Err(outside(
                &blob,
                "is not in strictly ascending ID order".into(),
            ));
        }
        let max = ids[ids.len() - 1].1;
        if let Some(claim) = blob.claim
            && (claim.kind, claim.min_id, claim.max_id, claim.count)
                != (kind, min, max, ids.len() as u64)
        {
            return Err(outside(
                &blob,
                "carries indexdata its elements contradict".into(),
            ));
        }
        out.push(Entry {
            blob,
            kind,
            min,
            max,
        });
    }
    Ok(out)
}

/// A step of the predicted output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Segment {
    /// Blob `entry` of input `input` copied through.
    Pass { input: usize, entry: Entry },
    /// A run of `members` blobs of `kind` re-encoded, its elements within
    /// `[min, reach]`.
    Run {
        kind: usize,
        min: i64,
        reach: i64,
        members: Vec<(usize, Entry)>,
    },
}

/// The predicted plan of `cat --dedupe` over `inputs` in argument order.
pub(crate) fn predict(inputs: &[&[Entry]]) -> Vec<Segment> {
    let mut all: Vec<(usize, Entry)> = inputs
        .iter()
        .enumerate()
        .flat_map(|(i, es)| es.iter().map(move |&e| (i, e)))
        .collect();
    // Stable: equal keys keep argument then file order.
    all.sort_by_key(|(_, e)| (e.kind, e.min));
    let mut out = Vec::new();
    let mut i = 0;
    while i < all.len() {
        let (kind, min) = (all[i].1.kind, all[i].1.min);
        let mut reach = all[i].1.max;
        let mut j = i + 1;
        while j < all.len() && all[j].1.kind == kind && all[j].1.min <= reach {
            reach = reach.max(all[j].1.max);
            j += 1;
        }
        if j == i + 1 {
            out.push(Segment::Pass {
                input: all[i].0,
                entry: all[i].1,
            });
        } else {
            out.push(Segment::Run {
                kind,
                min,
                reach,
                members: all[i..j].to_vec(),
            });
        }
        i = j;
    }
    out
}

/// What a plan says about the summary and the header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Shape {
    pub total: u64,
    pub passthrough: u64,
    pub rewritten: u64,
    /// A way blob is in a run, so ways lose their enrichments.
    pub way_rewritten: bool,
    /// A run with a passthrough of its own kind on both sides.
    pub bounded_runs: usize,
}

pub(crate) fn shape(plan: &[Segment]) -> Shape {
    let mut s = Shape {
        total: 0,
        passthrough: 0,
        rewritten: 0,
        way_rewritten: false,
        bounded_runs: 0,
    };
    for (i, seg) in plan.iter().enumerate() {
        match seg {
            Segment::Pass { .. } => {
                s.total += 1;
                s.passthrough += 1;
            }
            Segment::Run { kind, members, .. } => {
                s.total += members.len() as u64;
                s.rewritten += members.len() as u64;
                s.way_rewritten |= *kind == 1;
                let pass_of = |seg: Option<&Segment>| matches!(seg, Some(Segment::Pass { entry, .. }) if entry.kind == *kind);
                if i > 0 && pass_of(plan.get(i - 1)) && pass_of(plan.get(i + 1)) {
                    s.bounded_runs += 1;
                }
            }
        }
    }
    s
}

/// Hold `out`'s data blobs to `plan` over `inputs` (paths in argument
/// order). `Ok(Err(why))` names the first departure.
pub(crate) fn judge(
    out: &Path,
    inputs: &[&Path],
    plan: &[Segment],
) -> Result<Result<(), String>, Error> {
    let mut blobs = pbf_frames::Claims::open(out)?;
    let mut next = blobs.next_blob()?;
    for (n, seg) in plan.iter().enumerate() {
        match seg {
            Segment::Pass { input, entry } => {
                let Some(got) = next else {
                    return Ok(Err(format!(
                        "the output ends before passthrough segment {n} (input {} data blob {})",
                        input + 1,
                        entry.blob.index
                    )));
                };
                if !pbf_frames::same_payload(out, &got, inputs[*input], &entry.blob)? {
                    return Ok(Err(format!(
                        "output data blob {} is not input {} data blob {} copied through \
                         (segment {n})",
                        got.index,
                        input + 1,
                        entry.blob.index
                    )));
                }
                next = blobs.next_blob()?;
            }
            Segment::Run {
                kind, min, reach, ..
            } => {
                let mut consumed = 0usize;
                let mut last: Option<i64> = None;
                while let Some(got) = next {
                    // The blob after a run is inflated too: its own first kind
                    // and ID are the evidence that the run ended there, so
                    // the boundary is read from the output rather than
                    // assumed from the plan. One blob per run end, negligible
                    // beside osmium's whole-file decodes.
                    let ids = pbf_frames::blob_ids(out, got.payload_offset, got.datasize)?;
                    let Some(&(k, first)) = ids.first() else {
                        return Ok(Err(format!("output data blob {} is empty", got.index)));
                    };
                    if k != *kind || first > *reach {
                        break;
                    }
                    let inside = ids
                        .iter()
                        .all(|&(k, id)| k == *kind && id >= *min && id <= *reach);
                    let ascending =
                        ids.windows(2).all(|w| w[0].1 < w[1].1) && last.is_none_or(|l| l < first);
                    if !inside || !ascending {
                        return Ok(Err(format!(
                            "output data blob {} of run segment {n} is not wholly of kind {kind}, \
                             within IDs {min}..={reach} and ascending",
                            got.index
                        )));
                    }
                    if let Some(claim) = got.claim
                        && (claim.kind, claim.min_id, claim.max_id, claim.count)
                            != (k, first, ids[ids.len() - 1].1, ids.len() as u64)
                    {
                        return Ok(Err(format!(
                            "output data blob {} carries indexdata its elements contradict",
                            got.index
                        )));
                    }
                    last = Some(ids[ids.len() - 1].1);
                    consumed += 1;
                    next = blobs.next_blob()?;
                }
                if consumed == 0 {
                    return Ok(Err(format!(
                        "no output blob of kind {kind} within IDs {min}..={reach} for run \
                         segment {n}"
                    )));
                }
            }
        }
    }
    if let Some(extra) = next {
        return Ok(Err(format!(
            "output data blob {} follows the last predicted segment",
            extra.index
        )));
    }
    Ok(Ok(()))
}

/// The blob counts of the `Merged ...` summary line in `stderr`:
/// `(total, passthrough, rewritten)`, `None` if absent or unreadable.
pub(crate) fn summary_counts(stderr: &str) -> Option<(u64, u64, u64)> {
    stderr.lines().find_map(|line| {
        let rest = line.strip_prefix("Merged ")?;
        let (_, rest) = rest.split_once(" (")?;
        let (total, rest) = rest.split_once(" blobs: ")?;
        let (pass, rest) = rest.split_once(" passthrough, ")?;
        let (rewritten, _) = rest.split_once(" rewritten")?;
        Some((
            total.parse().ok()?,
            pass.parse().ok()?,
            rewritten.parse().ok()?,
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(index: u64, kind: usize, min: i64, max: i64) -> Entry {
        Entry {
            blob: ClaimedBlob {
                claim: None,
                index,
                payload_offset: 0,
                datasize: 0,
            },
            kind,
            min,
            max,
        }
    }

    /// Overlap is followed through the widest reach: A=[1..1000] takes
    /// B=[2..3] and C=[500..600] although B and C are disjoint.
    #[test]
    fn runs_follow_the_widest_reach() {
        let a = [e(0, 0, 1, 1000), e(1, 0, 2000, 3000)];
        let b = [e(0, 0, 2, 3), e(1, 0, 500, 600)];
        let plan = predict(&[&a, &b]);
        assert_eq!(plan.len(), 2);
        assert!(
            matches!(&plan[0], Segment::Run { reach: 1000, members, .. } if members.len() == 3)
        );
        assert!(matches!(&plan[1], Segment::Pass { input: 0, .. }));
    }

    /// A window bridging two disjoint blobs joins them in one run, flanked
    /// by passthroughs; runs never cross kinds.
    #[test]
    fn a_bridge_is_one_bounded_run() {
        let a = [
            e(0, 0, 1, 10),
            e(1, 0, 11, 20),
            e(2, 0, 21, 30),
            e(3, 0, 31, 40),
            e(4, 1, 1, 10),
        ];
        let w = [e(0, 0, 15, 25), e(1, 1, 5, 6)];
        let plan = predict(&[&a, &w]);
        let s = shape(&plan);
        assert_eq!((s.total, s.passthrough, s.rewritten), (7, 2, 5));
        assert_eq!(s.bounded_runs, 1);
        assert!(s.way_rewritten);
    }

    /// Equal first IDs keep argument order, so a self-merge pairs each blob
    /// with its twin and nothing passes through.
    #[test]
    fn a_self_merge_rewrites_everything() {
        let a = [e(0, 0, 1, 10), e(1, 0, 11, 20)];
        let plan = predict(&[&a, &a]);
        let s = shape(&plan);
        assert_eq!((s.passthrough, s.rewritten, s.bounded_runs), (0, 4, 0));
        match &plan[0] {
            Segment::Run { members, .. } => assert_eq!(members[0].0, 0),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_summary_counts_are_read() {
        let line = "Merged 10 elements: 5 nodes, 3 ways, 2 relations \
                    (7 blobs: 2 passthrough, 5 rewritten, 4 duplicates removed)";
        assert_eq!(summary_counts(line), Some((7, 2, 5)));
        assert_eq!(summary_counts("nothing"), None);
    }
}
