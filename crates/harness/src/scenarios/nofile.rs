//! Descriptor admission with caller-owned grants, independent of inherited
//! limits and ambient opens. Refused calls name a missing input (the runner
//! refuses the run if it exists) so a typed descriptor refusal also proves
//! admission preceded input acquisition. The external join receives 896
//! descriptors and fixed parallelism eight: its CPU cap is six and its
//! descriptor cap is three.
//! Real process-limit probing belongs in a separate child contract test;
//! these scenarios test command admission and sizing, not the host probe.

use super::{Scenario, account, assertion, published_once};
use crate::protocol::Outcome;
use crate::runner::{CallRecord, Done, Observation, Tap};
use crate::workload::{
    CLEAN_NONE, CallPlan, Command, DescriptorPool, ExecutorPlan, ExecutorPool, HostPlan, InputRole,
    Inputs, OneInput, OutputPlan, Parallelism, ProcessPlan, WRITE, Workload, no_overrides,
};
use pbfhogg::altw::{AltwOptions, IndexType};
use pbfhogg::debug::InstrumentSink;
use pbfhogg::host::PageCachePolicy;
use pbfhogg::{Decision, Effect, Overwrite};
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, PoisonError};

/// Exact caller grants used as scenario inputs, not process limits.
const CAT_GRANT: u64 = 1024;
const MULTI_GRANT: u64 = 6;
const ALTW_GRANT: u64 = 896;

/// The fixed parallelism and executor size of every descriptor scenario:
/// for the external join, a CPU cap of 6 stage-1 workers, above the 3 the
/// descriptor allowance gives.
const PARALLELISM: NonZeroUsize = NonZeroUsize::MIN.saturating_add(7);

/// A descriptor workload: one call on a pool of [`PARALLELISM`] threads
/// and a descriptor pool of `grant`, the global pool never started.
fn descriptor_workload(
    id: &'static str,
    description: &'static str,
    deadline_s: u64,
    grant: u64,
    mut call: CallPlan,
) -> Workload {
    let mut w = Workload::new(id, description, deadline_s);
    w.starts_global_pool = false;
    w.executor_pools = vec![ExecutorPool {
        id: "pool",
        threads: PARALLELISM.get(),
    }];
    w.fd_pools = vec![DescriptorPool {
        id: "grant",
        budget: grant,
    }];
    call.host = HostPlan {
        parallelism: Parallelism::Fixed(PARALLELISM),
        executor: ExecutorPlan::Pool("pool"),
        process: ProcessPlan::FdPool("grant"),
        page_cache: PageCachePolicy::Keep,
    };
    w.calls = vec![call];
    w
}

/// How many spellings of its one input the refused cat names.
const SPELLINGS: usize = 1100;

/// Records every marker its call passes.
struct Markers(Mutex<Vec<String>>);

impl InstrumentSink for Markers {
    fn marker(&self, name: &str) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(name.to_owned());
    }
    fn counter(&self, _name: &str, _value: i64) {}
}

impl Tap for Markers {
    fn release(&self) {}
}

impl Markers {
    fn new() -> Arc<Self> {
        Arc::new(Self(Mutex::new(Vec::new())))
    }

    fn saw(&self, name: &str) -> bool {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .any(|n| n == name)
    }
}

pub(crate) struct NofileCatRefused;

impl Scenario for NofileCatRefused {
    fn workload(&self) -> Workload {
        let mut w = descriptor_workload(
            "nofile-cat-refused",
            "cat of 1100 spellings of a missing path with a caller grant of 1024 descriptors, refused up front",
            120,
            CAT_GRANT,
            CallPlan::plain(
                "a",
                "cat 1100 spellings of a missing path -> out with a 1024 descriptor grant",
                Command::Cat {
                    inputs: Inputs::MissingSpellings { count: SPELLINGS },
                    output: OutputPlan::fresh("out"),
                    type_filter: None,
                    clean: CLEAN_NONE,
                    write: WRITE,
                    overrides: no_overrides(),
                },
            ),
        );
        w.assertions = vec![assertion(
            "a.refused",
            Some("a"),
            &["nofile.cat-refused.v1"],
        )];
        w
    }

    fn observe(&self) -> Box<dyn Observation> {
        Box::new(CatJudge(Markers::new()))
    }
}

struct CatJudge(Arc<Markers>);

impl Observation for CatJudge {
    fn tap(&self, _call: &str) -> Option<Arc<dyn Tap>> {
        Some(Arc::clone(&self.0) as Arc<dyn Tap>)
    }

    fn judge_call(&self, record: &CallRecord, done: &Done<'_>) -> Vec<Outcome> {
        let out_absent = super::absent(done.binding("out"));
        let scanned = self.0.saw("CAT_SCAN_START");
        let ok = record.outcome.error().is_some_and(|(kind, message)| {
            kind == "invalid_argument" && message.contains("descriptors")
        }) && published_once(record, Decision::RefusedError, "out", Effect::None)
            && matches!(out_absent, Ok(true))
            && !scanned;
        let detail = format!(
            "{}, output absent {out_absent:?}, CAT_SCAN_START passed {scanned}",
            account(record)
        );
        vec![Outcome::judge("a.refused", ok, detail)]
    }
}

pub(crate) struct NofileMultiExtractRefused;

impl Scenario for NofileMultiExtractRefused {
    fn workload(&self) -> Workload {
        let mut w = descriptor_workload(
            "nofile-multi-extract-refused",
            "single-pass two-region extract of a missing path with a caller grant of 6 descriptors, refused up front",
            120,
            MULTI_GRANT,
            CallPlan::plain(
                "a",
                "extract --simple of a missing path into out-a and out-b with a 6 descriptor grant",
                Command::ExtractMulti {
                    input: OneInput::Missing,
                    regions: vec![
                        ("12.4,55.6,12.7,55.8", "out-a"),
                        ("12.4,55.6,12.7,55.8", "out-b"),
                    ],
                    overwrite: Overwrite::Refuse,
                    strategy: pbfhogg::extract::ExtractStrategy::Simple,
                    set_bounds: false,
                    clean: CLEAN_NONE,
                    write: WRITE,
                    overrides: no_overrides(),
                },
            ),
        );
        w.assertions = vec![assertion(
            "a.refused",
            Some("a"),
            &["nofile.multi-extract-refused.v1"],
        )];
        w
    }

    fn observe(&self) -> Box<dyn Observation> {
        Box::new(MultiJudge(Markers::new()))
    }
}

struct MultiJudge(Arc<Markers>);

impl Observation for MultiJudge {
    fn tap(&self, _call: &str) -> Option<Arc<dyn Tap>> {
        Some(Arc::clone(&self.0) as Arc<dyn Tap>)
    }

    fn judge_call(&self, record: &CallRecord, done: &Done<'_>) -> Vec<Outcome> {
        let outs_absent: Vec<std::io::Result<bool>> = ["out-a", "out-b"]
            .iter()
            .map(|role| super::absent(done.binding(role)))
            .collect();
        let all_absent = outs_absent.iter().all(|a| matches!(a, Ok(true)));
        let scanned = self.0.saw("MULTI_EXTRACT_START");
        let refused_row = matches!(
            record.publications.as_slice(),
            [p] if p.ordinal == 1
                && p.decision == Decision::RefusedError
                && p.members == [("out-a", Effect::None), ("out-b", Effect::None)]
        );
        let ok = record.outcome.error().is_some_and(|(kind, message)| {
            // The count itself: two writers, one pin and the allowance of
            // eight, so a request without the per-region writers fails.
            kind == "invalid_argument" && message.contains("need 11 descriptors")
        }) && refused_row
            && all_absent
            && !scanned;
        let detail = format!(
            "{}, outputs absent {outs_absent:?}, MULTI_EXTRACT_START passed {scanned}, \
             descriptor grant {MULTI_GRANT}",
            account(record)
        );
        vec![Outcome::judge("a.refused", ok, detail)]
    }
}

pub(crate) struct NofileAltwExternal;

/// Sized from the caller's parallelism and the descriptor allowance, three
/// stage-1 workers ran and the join completed.
const ALTW_EXTERNAL: &[&str] = &["nofile.altw-external.v1"];

/// Records the counters stage 1 reports about its worker count.
struct Workers(Mutex<Vec<(String, i64)>>);

impl InstrumentSink for Workers {
    fn marker(&self, _name: &str) {}
    fn counter(&self, name: &str, value: i64) {
        if name.starts_with("extjoin_") {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((name.to_owned(), value));
        }
    }
}

impl Tap for Workers {
    fn release(&self) {}
}

impl Workers {
    fn get(&self, name: &str) -> Option<i64> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map(|(_, v)| *v)
    }
}

impl Scenario for NofileAltwExternal {
    fn workload(&self) -> Workload {
        let mut w = descriptor_workload(
            "nofile-altw-external",
            "external add-locations-to-ways with a caller grant of 896 descriptors completes with three stage-1 workers",
            1800,
            ALTW_GRANT,
            CallPlan::plain(
                "a",
                "add-locations-to-ways --index-type external input -> out",
                Command::Altw {
                    input: "input",
                    output: OutputPlan::fresh("out"),
                    options: AltwOptions {
                        keep_untagged_nodes: false,
                        write: WRITE,
                        index_type: IndexType::External,
                        inject_prepass: false,
                        ignore_missing_nodes: false,
                    },
                    overrides: no_overrides(),
                },
            ),
        );
        w.inputs = vec![InputRole::dataset("input", "indexed")];
        w.assertions = vec![
            assertion("a.outcome", Some("a"), ALTW_EXTERNAL),
            assertion("a.three-workers", Some("a"), ALTW_EXTERNAL),
        ];
        w
    }

    fn observe(&self) -> Box<dyn Observation> {
        Box::new(AltwJudge(Arc::new(Workers(Mutex::new(Vec::new())))))
    }
}

struct AltwJudge(Arc<Workers>);

impl Observation for AltwJudge {
    fn tap(&self, _call: &str) -> Option<Arc<dyn Tap>> {
        Some(Arc::clone(&self.0) as Arc<dyn Tap>)
    }

    fn judge_call(&self, record: &CallRecord, _done: &Done<'_>) -> Vec<Outcome> {
        let workers = &self.0;
        let (cpu, fd, ran, available) = (
            workers.get("extjoin_cpu_cap_workers"),
            workers.get("extjoin_fd_cap_workers"),
            workers.get("extjoin_num_shard_workers"),
            workers.get("extjoin_fd_available"),
        );
        let three = cpu == Some(6)
            && fd == Some(3)
            && ran == Some(3)
            && available == i64::try_from(ALTW_GRANT).ok();
        let detail = format!(
            "cpu cap {cpu:?}, descriptor cap {fd:?}, shard workers run {ran:?}, \
             descriptors available {available:?}: \
             workers are the smaller cap; caller grant {ALTW_GRANT}"
        );
        vec![
            super::installed_ok("a.outcome", record, "out"),
            Outcome::judge("a.three-workers", three, detail),
        ]
    }
}
