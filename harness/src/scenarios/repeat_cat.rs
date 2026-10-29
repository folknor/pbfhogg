//! `repeat-cat`: the same cat twice in a row, each call under its own
//! invocation and host, each with a CLI counterpart that must write an
//! equal output.

use super::Scenario;
use crate::protocol::{
    Arg, AssertionDecl, CallDecl, Counterpart, Outcome, Role, RoleDecl, ScenarioDecl,
};
use crate::runner::{CallRecord, Ctx};
use pbfhogg::cat::CleanAttrs;
use pbfhogg::write::{OutputBackend, WriteOptions};
use pbfhogg::writer::Compression;
use pbfhogg::{Decision, Effect, FieldOverride, HeaderOverrides, Output, Overwrite, Staging};
use serde_json::{Value, json};

pub(crate) struct RepeatCat;

/// The calls, each with the output role it owns.
const CALLS: [(&str, &str); 2] = [("a", "out-a"), ("b", "out-b")];

/// How the CLI's process policy differs from a call's.
const POLICY_DIFFERENCES: &[&str] = &[
    "the CLI evicts its input from the page cache as it reads; the call keeps it",
    "the CLI raises its soft RLIMIT_NOFILE when a command asks; the harness raised it at start",
    "the CLI caps malloc arenas when a command asks; the call leaves the allocator alone",
    "the CLI sizes work by probing the machine; the call gets fixed parallelism and no memory figure",
];

const ARGV_A: &[Arg] = &[
    Arg::Literal("cat"),
    Arg::Input("input"),
    Arg::Literal("-o"),
    Arg::Output("out-a"),
];
const ARGV_B: &[Arg] = &[
    Arg::Literal("cat"),
    Arg::Input("input"),
    Arg::Literal("-o"),
    Arg::Output("out-b"),
];

/// The write options both calls use, every field spelled out so a changed
/// library default cannot change the workload unseen.
const WRITE: WriteOptions = WriteOptions {
    compression: Compression::Zlib(6),
    backend: OutputBackend::Buffered,
    direct_reads: false,
    force: false,
};

const CLEAN: CleanAttrs = CleanAttrs {
    version: false,
    changeset: false,
    timestamp: false,
    uid: false,
    user: false,
};

fn overrides() -> HeaderOverrides {
    HeaderOverrides {
        generator: None,
        replication_timestamp: FieldOverride::Unchanged,
        replication_sequence_number: FieldOverride::Unchanged,
        replication_base_url: FieldOverride::Unchanged,
    }
}

impl Scenario for RepeatCat {
    fn decl(&self) -> ScenarioDecl {
        let call = |id, label, argv| CallDecl {
            id,
            label,
            cli: Some(Counterpart {
                argv,
                overwrite: Some(false),
                policy_differences: POLICY_DIFFERENCES,
            }),
        };
        ScenarioDecl {
            id: "repeat-cat",
            description: "cat the input twice in a row, each call under its own invocation",
            deadline_s: 600,
            calls: vec![
                call("a", "cat input -> out-a", ARGV_A),
                call("b", "cat input -> out-b", ARGV_B),
            ],
            roles: vec![
                RoleDecl {
                    id: "input",
                    role: Role::Dataset { variant: "indexed" },
                },
                RoleDecl {
                    id: "out-a",
                    role: Role::OutputPbf { owner: "a" },
                },
                RoleDecl {
                    id: "out-b",
                    role: Role::OutputPbf { owner: "b" },
                },
            ],
            assertions: vec![
                AssertionDecl {
                    id: "a.outcome",
                    call: Some("a"),
                },
                AssertionDecl {
                    id: "b.outcome",
                    call: Some("b"),
                },
                AssertionDecl {
                    id: "outputs-equal",
                    call: None,
                },
            ],
        }
    }

    fn plan(&self) -> Value {
        let call = |output: &str| {
            json!({
                "command": "cat",
                "inputs": ["input"],
                "output": output,
                "type_filter": null,
                "clean": { "version": false, "changeset": false, "timestamp": false,
                           "uid": false, "user": false },
                "write": { "compression": "zlib:6", "backend": "buffered",
                           "direct_reads": false, "force": false },
                "header_overrides": { "generator": null, "replication_timestamp": "unchanged",
                                      "replication_sequence_number": "unchanged",
                                      "replication_base_url": "unchanged" },
                "output_policy": { "overwrite": "refuse", "staging": "dir_with_backups_private" },
                "host": { "resources": "fixed", "parallelism": "machine",
                          "memory_available": null, "process_actions": "none",
                          "page_cache": "keep", "executor": "ambient" },
            })
        };
        json!({
            "schedule": { "order": ["a", "b"], "concurrency": "sequential",
                          "invocation": "one per call", "host": "one per call" },
            "setup": { "generated": [], "rayon_global_pool": "started before the first call" },
            "process": { "rlimit_nofile_soft": "raise_to_hard", "sigpipe": "inherited" },
            "calls": { "a": call("out-a"), "b": call("out-b") },
        })
    }

    fn run(&self, ctx: &mut Ctx<'_>) -> Result<Vec<Outcome>, String> {
        for (call, output) in CALLS {
            let record = ctx.call(call, |host, env| {
                let out = Output::with_staging(
                    env.binding(output),
                    Overwrite::Refuse,
                    Staging::DirWithBackups(env.private_dir()),
                    &host.diagnostics,
                )?;
                let input = env.binding("input");
                pbfhogg::cat::cat(&[input], out, None, &CLEAN, &WRITE, &overrides(), host).map(drop)
            })?;
            let judged = judge(call, output, &record);
            ctx.finish_call(&record, &[judged])?;
        }
        Ok(vec![outputs_equal(ctx)])
    }
}

/// `<call>.outcome`: the call returned ok, made no cancel, and decided one
/// publication, authorized, which installed its one output.
fn judge(call: &str, output: &'static str, record: &CallRecord) -> Outcome {
    let id = if call == "a" {
        "a.outcome"
    } else {
        "b.outcome"
    };
    let published = matches!(
        record.publications.as_slice(),
        [p] if p.decision == Decision::Authorized && p.members == [(output, Effect::Installed)]
    );
    let ok = record.outcome.is_ok() && record.cancel.is_none() && published;
    let detail = format!(
        "outcome {}, {} publications, cancel {}",
        record.outcome.to_json(),
        record.publications.len(),
        if record.cancel.is_none() {
            "none"
        } else {
            "made"
        },
    );
    Outcome::judge(id, ok, detail)
}

/// `outputs-equal`: the two outputs are byte-identical.
fn outputs_equal(ctx: &Ctx<'_>) -> Outcome {
    let read = |role| std::fs::read(ctx.binding(role));
    match (read("out-a"), read("out-b")) {
        (Ok(a), Ok(b)) => Outcome::judge(
            "outputs-equal",
            a == b,
            format!("out-a {} bytes, out-b {} bytes", a.len(), b.len()),
        ),
        (a, b) => Outcome::judge(
            "outputs-equal",
            false,
            format!("cannot read both outputs: {:?} {:?}", a.err(), b.err()),
        ),
    }
}
