//! `page-cache-keep`: the same digest of one generated input twice, one
//! after the other, first under `PageCachePolicy::Keep`, then under
//! `PageCachePolicy::Evict`.
//!
//! The observation is the library's `fadvise_input_evictions` counter in
//! each call's sealed snapshot: every `POSIX_FADV_DONTNEED` the library asks
//! for on an input is counted, issued, into the invocation of the call that
//! asked, at the crate's one `posix_fadvise` (`Advice::EvictInput`). So the
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

use super::concurrent_cat::{NODES_PER_BLOCK, write_nodes};
use super::{Scenario, host_json_paging, process_json};
use crate::protocol::{
    AssertionDecl, CallDecl, GeneratedKind, Outcome, Role, RoleDecl, STREAM_SEALED, ScenarioDecl,
};
use crate::runner::{CallRecord, CallSpec, Ctx, page_cache_name};
use pbfhogg::digest::{Digest, Form};
use pbfhogg::host::PageCachePolicy;
use serde_json::{Value, json};
use std::sync::Mutex;

pub(crate) struct PageCacheKeep;

/// Data blocks in the generated input.
const BLOCKS: u64 = 3;
/// Frames in the generated input: its header and its data blocks.
const FRAMES: u64 = BLOCKS + 1;

/// The calls: id, policy, outcome assertion.
const CALLS: [(&str, PageCachePolicy, &str); 2] = [
    ("keep", PageCachePolicy::Keep, "keep.outcome"),
    ("evict", PageCachePolicy::Evict, "evict.outcome"),
];

const EVICTIONS: &str = "fadvise_input_evictions";

impl Scenario for PageCacheKeep {
    fn decl(&self) -> ScenarioDecl {
        let call = |id, label| CallDecl {
            id,
            label,
            cli: None,
            stream: STREAM_SEALED,
        };
        let outcome = |id, call| AssertionDecl {
            id,
            call: Some(call),
            covers: &[],
        };
        ScenarioDecl {
            id: "page-cache-keep",
            description: "digest one input under PageCachePolicy::Keep, then under Evict, counting input evictions",
            deadline_s: 120,
            hotpath: false,
            isolation: false,
            calls: vec![
                call("keep", "digest input on PageCachePolicy::Keep"),
                call("evict", "digest input on PageCachePolicy::Evict"),
            ],
            roles: vec![RoleDecl {
                id: "input",
                role: Role::Generated {
                    kind: GeneratedKind::Pbf,
                },
            }],
            assertions: vec![
                outcome("keep.outcome", "keep"),
                outcome("evict.outcome", "evict"),
                AssertionDecl {
                    id: "keep.evicts-nothing",
                    call: Some("keep"),
                    covers: &["host.page-cache-keep.v1"],
                },
            ],
        }
    }

    fn plan(&self) -> Value {
        let call = |policy| {
            json!({
                "command": "digest",
                "inputs": ["input"],
                "form": "content",
                "direct_io": false,
                "host": host_json_paging(&json!("machine"), &json!("ambient"), &json!("none"),
                                         page_cache_name(policy)),
            })
        };
        json!({
            "schedule": { "order": ["keep", "evict"], "concurrency": "sequential",
                          "invocation": "one per call", "host": "one per call" },
            "setup": { "generated": [
                           { "role": "input",
                             "recipe": { "writer": "BlockBuilder and PbfWriter, zlib:6, sorted header",
                                         "blocks": BLOCKS, "nodes_per_block": NODES_PER_BLOCK,
                                         "node_ids": "1 upward", "tags": "none", "metadata": "none" } } ],
                       "rayon_global_pool": "started before the first call" },
            "process": process_json(self.nofile()),
            "calls": { "keep": call(PageCachePolicy::Keep), "evict": call(PageCachePolicy::Evict) },
        })
    }

    fn setup(&self, ctx: &Ctx<'_>) -> Result<(), String> {
        write_nodes(ctx.binding("input"), BLOCKS).map_err(|e| format!("writing input: {e}"))
    }

    fn run(&self, ctx: &mut Ctx<'_>) -> Result<Vec<Outcome>, String> {
        let mut done: Vec<(CallRecord, Option<Digest>)> = Vec::new();
        for (id, policy, _) in CALLS {
            let digest = Mutex::new(None);
            let mut spec = CallSpec::plain(id);
            spec.page_cache = policy;
            let record = ctx.call(spec, |host, env| {
                let d = pbfhogg::digest::digest(env.binding("input"), Form::Content, false, host)?;
                *digest
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(d);
                Ok(())
            })?;
            let digest = digest
                .into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            done.push((record, digest));
        }
        let nodes = BLOCKS * u64::try_from(NODES_PER_BLOCK).unwrap_or(0);
        let kept = done[0].0.sealed_counter(EVICTIONS);
        let evicted = done[1].0.sealed_counter(EVICTIONS);
        let mut evicts_nothing = Some(Outcome::judge(
            "keep.evicts-nothing",
            kept == Some(0) && evicted == Some(FRAMES),
            format!(
                "{EVICTIONS} in the sealed snapshots: keep {kept:?}, expected Some(0); \
                 evict {evicted:?}, expected Some({FRAMES}), one per frame of the input"
            ),
        ));
        let same = done[0].1.is_some() && done[0].1 == done[1].1;
        for (i, (record, digest)) in done.iter().enumerate() {
            let (_, _, outcome_id) = CALLS[i];
            let ok = record.outcome.is_ok()
                && record.publications.is_empty()
                && digest.is_some_and(|d| d.nodes.count == nodes)
                && same;
            let outcome = Outcome::judge(
                outcome_id,
                ok,
                format!(
                    "{}, nodes {:?} of {nodes}, digests of both calls equal {same}",
                    super::account(record),
                    digest.map(|d| d.nodes.count)
                ),
            );
            let mut results = vec![outcome];
            if i == 0 {
                results.extend(evicts_nothing.take());
            }
            ctx.finish_call(record, &results)?;
        }
        Ok(Vec::new())
    }
}
