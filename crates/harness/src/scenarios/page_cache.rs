//! `page-cache-keep`: the same digest of one generated input twice, one
//! after the other, first under `PageCachePolicy::Keep`, then under
//! `PageCachePolicy::Evict`.
//!
//! The observation is the library's `fadvise_input_evictions` counter in
//! each call's sealed snapshot: every `POSIX_FADV_DONTNEED` the library asks
//! for on an input is counted, issued, into the invocation of the call that
//! asked, through the library's one counted eviction (`fadvise::evict_input`,
//! which issues nothing it could not count); the library's uncounted
//! eviction takes only its own scratch files, by type. So the
//! count is attributable to the call, not read off page-cache residency,
//! which kernel reclaim and other processes move too. It is advice issued,
//! not pages dropped: the claim is that a call on `Keep` asks for no
//! eviction of its inputs.
//!
//! The `Evict` call is the contrast that shows the observation sees
//! evictions at all: digest reads through an evicting reader that advises
//! after every frame, header included, so its count is exactly the input's
//! frames. Both results stand in the one covering assertion, so a dead
//! counter cannot pass the capability. `cat` could not be the contrast: its
//! raw-frame walk never evicts under either policy.

use super::{Scenario, assertion};
use crate::protocol::Outcome;
use crate::runner::{CallRecord, Done, Observation};
use crate::workload::{
    CallPlan, Command, HostPlan, InputRole, NODES_PER_BLOCK, Recipe, Returned, Workload,
};
use pbfhogg::digest::Form;
use pbfhogg::host::PageCachePolicy;

pub(crate) struct PageCacheKeep;

/// Data blocks in the generated input.
const BLOCKS: u64 = 3;
/// Frames in the generated input: its header and its data blocks.
const FRAMES: u64 = BLOCKS + 1;

const EVICTIONS: &str = "fadvise_input_evictions";

fn digest(id: &'static str, label: &'static str, page_cache: PageCachePolicy) -> CallPlan {
    let mut call = CallPlan::plain(
        id,
        label,
        Command::Digest {
            input: "input",
            form: Form::Content,
            direct_io: false,
        },
    );
    call.host = HostPlan {
        page_cache,
        ..HostPlan::PLAIN
    };
    call
}

impl Scenario for PageCacheKeep {
    fn workload(&self) -> Workload {
        let mut w = Workload::new(
            "page-cache-keep",
            "digest one input under PageCachePolicy::Keep, then under Evict, counting input evictions",
            120,
        );
        w.inputs = vec![InputRole::generated(
            "input",
            Recipe::Nodes { blocks: BLOCKS },
        )];
        w.calls = vec![
            digest(
                "keep",
                "digest input on PageCachePolicy::Keep",
                PageCachePolicy::Keep,
            ),
            digest(
                "evict",
                "digest input on PageCachePolicy::Evict",
                PageCachePolicy::Evict,
            ),
        ];
        w.assertions = vec![
            assertion("keep.outcome", Some("keep"), &[]),
            assertion("evict.outcome", Some("evict"), &[]),
            assertion(
                "keep.evicts-nothing",
                Some("keep"),
                &["host.page-cache-keep.v1"],
            ),
        ];
        w
    }

    fn observe(&self) -> Box<dyn Observation> {
        Box::new(Judge)
    }
}

struct Judge;

fn digest_of(record: Option<&CallRecord>) -> Option<pbfhogg::digest::Digest> {
    match record.and_then(|r| r.returned) {
        Some(Returned::Digest(d)) => Some(d),
        _ => None,
    }
}

impl Observation for Judge {
    fn judge_call(&self, record: &CallRecord, done: &Done<'_>) -> Vec<Outcome> {
        let nodes = BLOCKS * u64::try_from(NODES_PER_BLOCK).unwrap_or(0);
        let keep = digest_of(done.record("keep"));
        let evict = digest_of(done.record("evict"));
        let same = keep.is_some() && keep == evict;
        let digest = digest_of(Some(record));
        let ok = record.outcome.is_ok()
            && record.publications.is_empty()
            && digest.is_some_and(|d| d.nodes.count == nodes)
            && same;
        let outcome_id = if record.id == "keep" {
            "keep.outcome"
        } else {
            "evict.outcome"
        };
        let mut results = vec![Outcome::judge(
            outcome_id,
            ok,
            format!(
                "{}, nodes {:?} of {nodes}, digests of both calls equal {same}",
                super::account(record),
                digest.map(|d| d.nodes.count)
            ),
        )];
        if record.id == "keep" {
            let kept = record.sealed_counter(EVICTIONS);
            let evicted = done
                .record("evict")
                .and_then(|r| r.sealed_counter(EVICTIONS));
            results.push(Outcome::judge(
                "keep.evicts-nothing",
                kept == Some(0) && evicted == Some(FRAMES),
                format!(
                    "{EVICTIONS} in the sealed snapshots: keep {kept:?}, expected Some(0); \
                     evict {evicted:?}, expected Some({FRAMES}), one per frame of the input"
                ),
            ));
        }
        results
    }
}
