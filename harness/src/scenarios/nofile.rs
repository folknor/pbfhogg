//! The descriptor-limit scenarios: single calls on `NoProcessActions` with
//! the process's soft `RLIMIT_NOFILE` set to 1024 at launch (hard limit
//! untouched, read back, reported in each context), before any library
//! work.
//!
//! `nofile-cat-refused`: cat of 1100 distinct spellings of one path asks
//! for 1108 descriptors, more than the soft limit allows whatever is open,
//! so it is refused before any input is pinned. The path does not exist:
//! had the call opened or read an input before its descriptor admission,
//! that would have failed first with a not-found I/O error, so the typed
//! descriptor refusal (with `CAT_SCAN_START` never passed) shows admission
//! ran before any input was acquired. No counterpart: the CLI raises its
//! soft limit whenever a command asks, so it would not refuse.
//!
//! `nofile-multi-extract-refused`: the call first opens read-only
//! descriptors on its private directory until 8 remain free under the soft
//! limit (an embedding process near its limit), then builds two outputs
//! (two staging descriptors, leaving 6) and runs a simple two-region
//! extract of a missing path, which asks for 11: a writer per region, the
//! pin and an allowance of 8. Refused before the input is pinned, by the
//! same argument as the refused cat (a not-found error would have come
//! first otherwise, and `MULTI_EXTRACT_START` is never passed), and both
//! members of the set are decided refused with no effect. The fillers stand
//! in for many regions: the brokkr protocol binds output roles one by one,
//! so the scenario shrinks the free count rather than growing the region
//! count. No counterpart, as for cat.
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
use crate::protocol::{
    Arg, AssertionDecl, CallDecl, Counterpart, Outcome, RoleDecl, STREAM_SEALED, ScenarioDecl,
};
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
            description: "cat of 1100 spellings of a missing path at soft RLIMIT_NOFILE 1024, refused up front",
            deadline_s: 120,
            hotpath: false,
            isolation: false,
            calls: vec![CallDecl {
                id: "a",
                label: "cat 1100 spellings of a missing path -> out at soft nofile 1024",
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
        let markers = Arc::new(Markers(Mutex::new(Vec::new())));
        let mut spec = CallSpec::plain("a");
        spec.tap = Some(Arc::clone(&markers) as Arc<dyn Tap>);
        let mut missing_exists = false;
        let record = ctx.call(spec, |host, env| {
            // Raw spellings: `dir/./name`, `dir/././name`, ... Each is its
            // own pin (cat compares spellings byte for byte).
            let dir = env.private_dir();
            missing_exists = !super::absent(&dir.join(MISSING));
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
        let out_absent = super::absent(ctx.binding("out"));
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

/// Descriptors left free under the soft limit when the call starts building
/// its outputs: the two staging descriptors take two, leaving six, under
/// the eleven the single-pass extract asks for (two writers, one pin, an
/// allowance of eight).
const MULTI_FREE: u64 = 8;

/// The two output roles of the refused multi-extract.
const MULTI_OUTPUTS: [&str; 2] = ["out-a", "out-b"];

impl Scenario for NofileMultiExtractRefused {
    fn decl(&self) -> ScenarioDecl {
        ScenarioDecl {
            id: "nofile-multi-extract-refused",
            description: "single-pass two-region extract of a missing path with 8 descriptors free under soft RLIMIT_NOFILE 1024, refused up front",
            deadline_s: 120,
            hotpath: false,
            isolation: false,
            calls: vec![CallDecl {
                id: "a",
                label: "extract --simple of a missing path into out-a and out-b with 8 descriptors free",
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
            "setup": { "generated": [], "rayon_global_pool": "started before the first call" },
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
                "descriptors_free": format!(
                    "{MULTI_FREE} under the soft limit before the outputs are built, the rest \
                     held by the call's own opens of its private directory"
                ),
                "host": host_json(&json!("machine"), &json!("ambient"), &json!("none")),
            } },
        })
    }

    fn nofile(&self) -> Nofile {
        Nofile::Soft(SOFT)
    }

    fn run(&self, ctx: &mut Ctx<'_>) -> Result<Vec<Outcome>, String> {
        let markers = Arc::new(Markers(Mutex::new(Vec::new())));
        let mut spec = CallSpec::plain("a");
        spec.tap = Some(Arc::clone(&markers) as Arc<dyn Tap>);
        let mut missing_exists = false;
        let mut filled = None;
        let record = ctx.call(spec, |host, env| {
            let dir = env.private_dir();
            let missing = dir.join(MISSING);
            missing_exists = !super::absent(&missing);
            // Held to the end of the call: every descriptor but MULTI_FREE
            // under the soft limit.
            let fillers = fill_descriptors(dir, MULTI_FREE)?;
            filled = Some(fillers.len());
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
            drop(fillers);
            result.map(drop)
        })?;
        let outs_absent = MULTI_OUTPUTS
            .iter()
            .all(|role| super::absent(ctx.binding(role)));
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
             missing input existed {missing_exists}, filler descriptors {filled:?}",
            account(&record)
        );
        ctx.finish_call(&record, &[Outcome::judge("a.refused", ok, detail)])?;
        Ok(Vec::new())
    }
}

/// Open read-only descriptors on `dir` until only `free` remain under the
/// soft limit, counting the open ones in `/proc/self/fd` (less the one the
/// count itself holds). Opens nothing when fewer are free already.
fn fill_descriptors(dir: &std::path::Path, free: u64) -> pbfhogg::Result<Vec<std::fs::File>> {
    let entries = std::fs::read_dir("/proc/self/fd")?
        .count()
        .saturating_sub(1);
    let open = u64::try_from(entries).unwrap_or(u64::MAX);
    let target = SOFT.saturating_sub(free);
    let mut fillers = Vec::new();
    for _ in open..target {
        fillers.push(std::fs::File::open(dir)?);
    }
    Ok(fillers)
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
        let (cpu, fd, ran, available) = (
            workers.get("extjoin_cpu_cap_workers"),
            workers.get("extjoin_fd_cap_workers"),
            workers.get("extjoin_num_shard_workers"),
            workers.get("extjoin_fd_available"),
        );
        let three = cpu == Some(6) && fd == Some(3) && ran == Some(3);
        let detail = format!(
            "cpu cap {cpu:?}, descriptor cap {fd:?}, shard workers run {ran:?}, \
             descriptors available {available:?}: \
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
