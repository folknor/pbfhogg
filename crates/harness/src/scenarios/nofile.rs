//! Descriptor admission with caller-owned grants, independent of inherited
//! limits and ambient opens. Refused calls name missing inputs so a typed
//! descriptor refusal also proves admission preceded input acquisition.
//! The external join receives 896 descriptors and fixed parallelism eight:
//! its CPU cap is six and its descriptor cap is three.
//! Real process-limit probing belongs in a separate child contract test;
//! these scenarios test command admission and sizing, not the host probe.

use super::{
    CLEAN_NONE, Scenario, WRITE, account, cat_plan, host_json, output_policy_json, overrides,
    overrides_json, process_json, published_once, write_json,
};
use crate::protocol::{
    Arg, AssertionDecl, CallDecl, Counterpart, Outcome, RoleDecl, STREAM_SEALED, ScenarioDecl,
};
use crate::runner::{CallSpec, Ctx, Exec, Process, Tap};
use pbfhogg::altw::{AltwOptions, IndexType};
use pbfhogg::debug::InstrumentSink;
use pbfhogg::{Decision, Effect};
use serde_json::{Value, json};
use std::ffi::OsString;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

/// Exact caller grants used as scenario inputs, not process limits.
const CAT_GRANT: u64 = 1024;
const MULTI_GRANT: u64 = 6;
const ALTW_GRANT: u64 = 896;

/// The same explicit resources and executor for each descriptor scenario.
fn call_spec(grant: u64) -> Result<CallSpec, String> {
    let mut spec = CallSpec::plain("a");
    spec.parallelism = NonZeroUsize::new(ALTW_PARALLELISM);
    spec.executor = Exec::Pool(Arc::new(
        rayon::ThreadPoolBuilder::new()
            .num_threads(ALTW_PARALLELISM)
            .build()
            .map_err(|e| format!("build descriptor scenario pool: {e}"))?,
    ));
    spec.process = Process::Pool {
        actions: Arc::new(pbfhogg::host::FdPool::new(grant)),
        budget: grant,
    };
    Ok(spec)
}

fn descriptor_host(grant: u64) -> Value {
    let mut host = host_json(&json!(ALTW_PARALLELISM), &json!("pool"), &json!("fd_pool"));
    host["fd_pool_budget"] = json!(grant);
    host["executor_threads"] = json!(ALTW_PARALLELISM);
    host
}

/// How many spellings of its one input the refused cat names.
const SPELLINGS: usize = 1100;

pub(crate) struct NofileCatRefused;

impl Scenario for NofileCatRefused {
    fn decl(&self) -> ScenarioDecl {
        ScenarioDecl {
            id: "nofile-cat-refused",
            description: "cat of 1100 spellings of a missing path with a caller grant of 1024 descriptors, refused up front",
            deadline_s: 120,
            hotpath: false,
            isolation: false,
            calls: vec![CallDecl {
                id: "a",
                label: "cat 1100 spellings of a missing path -> out with a 1024 descriptor grant",
                cli: None,
                stream: STREAM_SEALED,
            }],
            roles: vec![RoleDecl::output("out", "a")],
            assertions: vec![AssertionDecl {
                id: "a.refused",
                call: Some("a"),
                covers: &["nofile.cat-refused.v1"],
            }],
        }
    }

    fn plan(&self) -> Value {
        let inputs = json!({ "path": "absent.osm.pbf in the call's private directory, never created",
                             "spellings": SPELLINGS,
                             "spelling": "k ./ segments before the file name, k from 0" });
        let mut call = cat_plan(&inputs, "out", false);
        call["host"] = descriptor_host(CAT_GRANT);
        json!({
            "schedule": { "order": ["a"], "concurrency": "sequential",
                          "invocation": "one per call", "host": "one per call" },
            "setup": { "generated": [], "rayon_global_pool": "not started" },
            "process": process_json(self.nofile()),
            "calls": { "a": call },
        })
    }

    fn starts_global_pool(&self) -> bool {
        false
    }

    fn run(&self, ctx: &mut Ctx<'_>) -> Result<Vec<Outcome>, String> {
        let markers = Arc::new(Markers(Mutex::new(Vec::new())));
        let mut spec = call_spec(CAT_GRANT)?;
        spec.tap = Some(Arc::clone(&markers) as Arc<dyn Tap>);
        let mut missing_exists = false;
        let record = ctx.call(spec, |host, env| {
            // Raw spellings: `dir/./name`, `dir/././name`, ... Each is its
            // own pin (cat compares spellings byte for byte).
            let dir = env.private_dir();
            missing_exists = !super::absent(&dir.join(MISSING))?;
            let spellings: Vec<PathBuf> = (0..SPELLINGS)
                .map(|k| {
                    let mut path = OsString::from(dir.as_os_str());
                    path.push("/");
                    for _ in 0..k {
                        path.push("./");
                    }
                    path.push(MISSING);
                    PathBuf::from(path)
                })
                .collect();
            let inputs: Vec<&std::path::Path> = spellings.iter().map(PathBuf::as_path).collect();
            super::cat_into(env, &inputs, "out", None, &CLEAN_NONE, host).map(drop)
        })?;
        let out_absent =
            super::absent(ctx.binding("out")).map_err(|e| format!("inspect output: {e}"))?;
        let scanned = markers.saw("CAT_SCAN_START");
        let ok = record.outcome.error().is_some_and(|(kind, message)| {
            kind == "invalid_argument" && message.contains("descriptors")
        }) && published_once(&record, Decision::RefusedError, "out", Effect::None)
            && out_absent
            && !scanned
            && !missing_exists;
        let detail = format!(
            "{}, output absent {out_absent}, CAT_SCAN_START passed {scanned}, \
             missing input existed {missing_exists}",
            account(&record)
        );
        ctx.finish_call(&record, &[Outcome::judge("a.refused", ok, detail)])?;
        Ok(Vec::new())
    }
}

/// The input the refused cat names, under its private directory, never
/// created.
const MISSING: &str = "absent.osm.pbf";

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
    fn saw(&self, name: &str) -> bool {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .any(|n| n == name)
    }
}

pub(crate) struct NofileMultiExtractRefused;

/// The two output roles of the refused multi-extract.
const MULTI_OUTPUTS: [&str; 2] = ["out-a", "out-b"];

impl Scenario for NofileMultiExtractRefused {
    fn decl(&self) -> ScenarioDecl {
        ScenarioDecl {
            id: "nofile-multi-extract-refused",
            description: "single-pass two-region extract of a missing path with a caller grant of 6 descriptors, refused up front",
            deadline_s: 120,
            hotpath: false,
            isolation: false,
            calls: vec![CallDecl {
                id: "a",
                label: "extract --simple of a missing path into out-a and out-b with a 6 descriptor grant",
                cli: None,
                stream: STREAM_SEALED,
            }],
            roles: vec![
                RoleDecl::output("out-a", "a"),
                RoleDecl::output("out-b", "a"),
            ],
            assertions: vec![AssertionDecl {
                id: "a.refused",
                call: Some("a"),
                covers: &["nofile.multi-extract-refused.v1"],
            }],
        }
    }

    fn plan(&self) -> Value {
        json!({
            "schedule": { "order": ["a"], "concurrency": "sequential",
                          "invocation": "one per call", "host": "one per call" },
            "setup": { "generated": [], "rayon_global_pool": "not started" },
            "process": process_json(self.nofile()),
            "calls": { "a": {
                "command": "extract",
                "input": "absent.osm.pbf in the call's private directory, never created",
                "outputs": MULTI_OUTPUTS,
                "regions": "one bbox per output, 12.4,55.6,12.7,55.8",
                "strategy": "simple",
                "set_bounds": false,
                "clean": super::clean_json(false),
                "write": write_json(),
                "header_overrides": overrides_json(),
                "output_policy": output_policy_json("refuse"),
                "host": descriptor_host(MULTI_GRANT),
            } },
        })
    }

    fn starts_global_pool(&self) -> bool {
        false
    }

    fn run(&self, ctx: &mut Ctx<'_>) -> Result<Vec<Outcome>, String> {
        let markers = Arc::new(Markers(Mutex::new(Vec::new())));
        let mut spec = call_spec(MULTI_GRANT)?;
        spec.tap = Some(Arc::clone(&markers) as Arc<dyn Tap>);
        let mut missing_exists = false;
        let record = ctx.call(spec, |host, env| {
            let dir = env.private_dir();
            let missing = dir.join(MISSING);
            missing_exists = !super::absent(&missing)?;
            let outputs = pbfhogg::OutputSet::files_with_staging(
                MULTI_OUTPUTS.iter().map(|role| env.binding(role)),
                pbfhogg::Overwrite::Refuse,
                pbfhogg::Staging::DirWithBackups(dir),
                &host.diagnostics,
            )?;
            let bbox = pbfhogg::extract::parse_bbox("12.4,55.6,12.7,55.8")?;
            let regions: Vec<pbfhogg::extract::Region> = MULTI_OUTPUTS
                .iter()
                .map(|_| pbfhogg::extract::Region::Bbox(bbox))
                .collect();
            let result = pbfhogg::extract::extract_multi(
                missing.as_path(),
                &regions,
                outputs,
                pbfhogg::extract::ExtractStrategy::Simple,
                false,
                &CLEAN_NONE,
                &WRITE,
                &overrides(),
                host,
            );
            result.map(drop)
        })?;
        let mut outs_absent = true;
        for role in MULTI_OUTPUTS {
            outs_absent &= super::absent(ctx.binding(role))
                .map_err(|e| format!("inspect output {role}: {e}"))?;
        }
        let scanned = markers.saw("MULTI_EXTRACT_START");
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
            && outs_absent
            && !scanned
            && !missing_exists;
        let detail = format!(
            "{}, outputs absent {outs_absent}, MULTI_EXTRACT_START passed {scanned}, \
             missing input existed {missing_exists}, descriptor grant {MULTI_GRANT}",
            account(&record)
        );
        ctx.finish_call(&record, &[Outcome::judge("a.refused", ok, detail)])?;
        Ok(Vec::new())
    }
}

pub(crate) struct NofileAltwExternal;

/// The fixed parallelism: a CPU cap of 6 stage-1 workers, above the 3 the
/// descriptor allowance gives.
const ALTW_PARALLELISM: usize = 8;

/// Sized from the caller's parallelism and the descriptor allowance, three
/// stage-1 workers ran and the join completed.
const ALTW_EXTERNAL: &[&str] = &["nofile.altw-external.v1"];

const ARGV: &[Arg] = &[
    Arg::Literal("add-locations-to-ways"),
    Arg::Input("input"),
    Arg::Literal("--index-type"),
    Arg::Literal("external"),
    Arg::Literal("-o"),
    Arg::Output("out"),
];

const ALTW_DIFFERENCES: &[&str] = &[
    "the CLI runs on PageCachePolicy::Evict; the call runs on Keep",
    "the CLI probes real descriptor availability; the call uses a caller grant of 896",
    "the CLI sizes work by probing the machine; the call gets fixed parallelism 8 and no memory figure",
];

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
    fn decl(&self) -> ScenarioDecl {
        ScenarioDecl {
            id: "nofile-altw-external",
            description: "external add-locations-to-ways with a caller grant of 896 descriptors completes with three stage-1 workers",
            deadline_s: 1800,
            hotpath: false,
            isolation: false,
            calls: vec![CallDecl {
                id: "a",
                label: "add-locations-to-ways --index-type external input -> out",
                cli: Some(Counterpart {
                    argv: ARGV,
                    scratch: true,
                    expect: crate::protocol::Expect::Output,
                    overwrite: Some(false),
                    policy_differences: ALTW_DIFFERENCES,
                }),
                stream: STREAM_SEALED,
            }],
            roles: vec![
                RoleDecl::dataset("input", "indexed"),
                RoleDecl::output("out", "a"),
            ],
            assertions: vec![
                AssertionDecl {
                    id: "a.outcome",
                    call: Some("a"),
                    covers: ALTW_EXTERNAL,
                },
                AssertionDecl {
                    id: "a.three-workers",
                    call: Some("a"),
                    covers: ALTW_EXTERNAL,
                },
            ],
        }
    }

    fn plan(&self) -> Value {
        json!({
            "schedule": { "order": ["a"], "concurrency": "sequential",
                          "invocation": "one per call", "host": "one per call" },
            "setup": { "generated": [], "rayon_global_pool": "not started" },
            "process": process_json(self.nofile()),
            "calls": { "a": {
                "command": "add-locations-to-ways",
                "input": "input",
                "output": "out",
                "index_type": "external",
                "keep_untagged_nodes": false,
                "inject_prepass": false,
                "ignore_missing_nodes": false,
                "write": write_json(),
                "header_overrides": overrides_json(),
                "output_policy": output_policy_json("refuse"),
                "scratch": "a run scratch in the private directory",
                "host": descriptor_host(ALTW_GRANT),
            } },
        })
    }

    fn starts_global_pool(&self) -> bool {
        false
    }

    fn run(&self, ctx: &mut Ctx<'_>) -> Result<Vec<Outcome>, String> {
        let workers = Arc::new(Workers(Mutex::new(Vec::new())));
        let mut spec = call_spec(ALTW_GRANT)?;
        spec.tap = Some(Arc::clone(&workers) as Arc<dyn Tap>);
        let record = ctx.call(spec, |host, env| {
            let out = super::output(env, "out", pbfhogg::Overwrite::Refuse, host)?;
            let scratch = pbfhogg::scratch::RunScratch::create(env.private_dir(), host)?;
            let options = AltwOptions {
                keep_untagged_nodes: false,
                write: WRITE,
                index_type: IndexType::External,
                inject_prepass: false,
                ignore_missing_nodes: false,
            };
            pbfhogg::altw::add_locations_to_ways(
                env.binding("input"),
                out,
                &options,
                &overrides(),
                host,
                scratch,
            )
            .map(drop)
        })?;
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
        let results = [
            super::installed_ok("a.outcome", &record, "out"),
            Outcome::judge("a.three-workers", three, detail),
        ];
        ctx.finish_call(&record, &results)?;
        Ok(Vec::new())
    }
}
