//! The descriptor-limit scenarios: single calls on `NoProcessActions` with
//! the process's soft `RLIMIT_NOFILE` set to 1024 at launch (hard limit
//! untouched, read back, reported in each context), before any library
//! work.
//!
//! `nofile-cat-refused`: cat of 1100 distinct spellings of one input asks
//! for 1108 descriptors, more than the soft limit allows whatever is open,
//! so it is refused before any input is pinned. No counterpart: the CLI
//! raises its soft limit whenever a command asks, so it would not refuse.
//!
//! `nofile-altw-external`: the external join at fixed parallelism 8 (a CPU
//! cap of 6 stage-1 workers) is held to 3 by the descriptor allowance,
//! `(available - 64) / 256`, and completes. That needs between 832 and
//! 1087 descriptors available at stage 1, so a launch that already holds
//! more than about 190 fails the assertion, naming the allowance it saw: a
//! launch condition, not a worker-selection defect.

use super::{
    CLEAN_NONE, Scenario, WRITE, account, cat_plan, host_json, output_policy_json, overrides,
    overrides_json, process_json, published_once, write_json,
};
use crate::process::Nofile;
use crate::protocol::{Arg, AssertionDecl, CallDecl, Counterpart, Outcome, RoleDecl, ScenarioDecl};
use crate::runner::{CallSpec, Ctx, Tap};
use pbfhogg::altw::{AltwOptions, IndexType};
use pbfhogg::debug::InstrumentSink;
use pbfhogg::{Decision, Effect};
use serde_json::{Value, json};
use std::ffi::OsString;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

/// The soft limit both scenarios run under.
const SOFT: u64 = 1024;

/// How many spellings of its one input the refused cat names.
const SPELLINGS: usize = 1100;

pub(crate) struct NofileCatRefused;

impl Scenario for NofileCatRefused {
    fn decl(&self) -> ScenarioDecl {
        ScenarioDecl {
            id: "nofile-cat-refused",
            description: "cat of 1100 distinct input spellings at soft RLIMIT_NOFILE 1024, refused up front",
            deadline_s: 120,
            hotpath: false,
            isolation: false,
            calls: vec![CallDecl {
                id: "a",
                label: "cat 1100 spellings of input -> out at soft nofile 1024",
                cli: None,
            }],
            roles: vec![
                RoleDecl::dataset("input", "indexed"),
                RoleDecl::output("out", "a"),
            ],
            assertions: vec![AssertionDecl {
                id: "a.refused",
                call: Some("a"),
            }],
        }
    }

    fn plan(&self) -> Value {
        let inputs = json!({ "role": "input", "spellings": SPELLINGS,
                             "spelling": "k ./ segments before the file name, k from 0" });
        json!({
            "schedule": { "order": ["a"], "concurrency": "sequential",
                          "invocation": "one per call", "host": "one per call" },
            "setup": { "generated": [], "rayon_global_pool": "started before the first call" },
            "process": process_json(self.nofile()),
            "calls": { "a": cat_plan(&inputs, "out", false) },
        })
    }

    fn nofile(&self) -> Nofile {
        Nofile::Soft(SOFT)
    }

    fn run(&self, ctx: &mut Ctx<'_>) -> Result<Vec<Outcome>, String> {
        let input = ctx.binding("input");
        let (Some(dir), Some(name)) = (input.parent(), input.file_name()) else {
            return Err("the input has no directory".to_owned());
        };
        // Raw spellings: `dir/./name`, `dir/././name`, ... Each is its own
        // pin (cat compares spellings byte for byte), none is opened.
        let spellings: Vec<PathBuf> = (0..SPELLINGS)
            .map(|k| {
                let mut path = OsString::from(dir.as_os_str());
                path.push("/");
                for _ in 0..k {
                    path.push("./");
                }
                path.push(name);
                PathBuf::from(path)
            })
            .collect();
        let record = ctx.call(CallSpec::plain("a"), |host, env| {
            let inputs: Vec<&std::path::Path> = spellings.iter().map(PathBuf::as_path).collect();
            super::cat_into(env, &inputs, "out", None, &CLEAN_NONE, host).map(drop)
        })?;
        let out_absent = super::absent(ctx.binding("out"));
        let ok = record.outcome.error().is_some_and(|(kind, message)| {
            kind == "invalid_argument" && message.contains("descriptors")
        }) && published_once(&record, Decision::RefusedError, "out", Effect::None)
            && out_absent;
        let detail = format!("{}, output absent {out_absent}", account(&record));
        ctx.finish_call(&record, &[Outcome::judge("a.refused", ok, detail)])?;
        Ok(Vec::new())
    }
}

pub(crate) struct NofileAltwExternal;

/// The fixed parallelism: a CPU cap of 6 stage-1 workers, above the 3 the
/// descriptor allowance gives.
const ALTW_PARALLELISM: usize = 8;

const ARGV: &[Arg] = &[
    Arg::Literal("add-locations-to-ways"),
    Arg::Input("input"),
    Arg::Literal("--index-type"),
    Arg::Literal("external"),
    Arg::Literal("-o"),
    Arg::Output("out"),
];

const ALTW_DIFFERENCES: &[&str] = &[
    "the CLI evicts its input from the page cache as it reads; the call keeps it",
    "the CLI raises its soft RLIMIT_NOFILE when a command asks, so it may run more stage-1 workers; the call stays at 1024",
    "the CLI caps malloc arenas when a command asks; the call leaves the allocator alone",
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
            description: "external add-locations-to-ways at soft RLIMIT_NOFILE 1024 completes with three stage-1 workers",
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
            }],
            roles: vec![
                RoleDecl::dataset("input", "indexed"),
                RoleDecl::output("out", "a"),
            ],
            assertions: vec![
                AssertionDecl {
                    id: "a.outcome",
                    call: Some("a"),
                },
                AssertionDecl {
                    id: "a.three-workers",
                    call: Some("a"),
                },
            ],
        }
    }

    fn plan(&self) -> Value {
        json!({
            "schedule": { "order": ["a"], "concurrency": "sequential",
                          "invocation": "one per call", "host": "one per call" },
            "setup": { "generated": [], "rayon_global_pool": "started before the first call" },
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
                "host": host_json(&json!(ALTW_PARALLELISM), &json!("ambient"), &json!("none")),
            } },
        })
    }

    fn nofile(&self) -> Nofile {
        Nofile::Soft(SOFT)
    }

    fn run(&self, ctx: &mut Ctx<'_>) -> Result<Vec<Outcome>, String> {
        let workers = Arc::new(Workers(Mutex::new(Vec::new())));
        let mut spec = CallSpec::plain("a");
        spec.parallelism = NonZeroUsize::new(ALTW_PARALLELISM);
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
        let (cpu, fd, available) = (
            workers.get("extjoin_cpu_cap_workers"),
            workers.get("extjoin_fd_cap_workers"),
            workers.get("extjoin_fd_available"),
        );
        let three = cpu == Some(6) && fd == Some(3);
        let detail = format!(
            "cpu cap {cpu:?}, descriptor cap {fd:?}, descriptors available {available:?}: \
             workers are the smaller cap; three need 832 to 1087 available at soft {SOFT}"
        );
        let results = [
            super::installed_ok("a.outcome", &record, "out"),
            Outcome::judge("a.three-workers", three, detail),
        ];
        ctx.finish_call(&record, &results)?;
        Ok(Vec::new())
    }
}
