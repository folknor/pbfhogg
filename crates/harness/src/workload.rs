//! A scenario's workload as one typed value: its inputs and how setup makes
//! them, the resources its calls share, its schedule, and every call's
//! command and host. The plan document, the describe entry's calls and
//! roles, setup, every call's host and context, and the library call itself
//! are all derived from this one value, so a scenario cannot run a workload
//! its plan does not name: the runner, not the scenario, builds the
//! resources, drives the schedule and makes each call.
//!
//! What stays handwritten is observation: a call's tap, a descriptor
//! observer, and the judgments. Their effect on a call is described by the
//! call's `notes`, which nothing checks against the code.
//!
//! Each conversion of a library option into the plan destructures it
//! without `..` and matches its enums exhaustively, so a field or variant
//! added to the library is a compile error here until it is given a
//! spelling. A non-exhaustive library enum's unknown variant refuses the plan.

use crate::process::Nofile;
use crate::protocol::{AssertionDecl, CallDecl, GeneratedKind, Role, RoleDecl, ScenarioDecl};
use pbfhogg::altw::{AltwOptions, IndexType};
use pbfhogg::cat::CleanAttrs;
use pbfhogg::digest::Form;
use pbfhogg::extract::ExtractStrategy;
use pbfhogg::host::{Host, PageCachePolicy};
use pbfhogg::write::{OutputBackend, WriteOptions};
use pbfhogg::writer::Compression;
use pbfhogg::{FieldOverride, HeaderOverrides, Overwrite, TypeFilter};
use serde_json::{Value, json};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The name of the input a refused call names, under the launch's private
/// directory. The runner refuses the run if it exists before the call.
pub(crate) const MISSING: &str = "absent.osm.pbf";

/// Nodes per block of a generated node input.
pub(crate) const NODES_PER_BLOCK: i64 = 100;
/// A generated node input's compression, header sort flag and coordinate
/// step: setup writes with these and the plan spells them.
const NODES_COMPRESSION: Compression = Compression::Zlib(6);
const NODES_SORTED: bool = true;
const NODES_COORD_STEP: i32 = 1000;

/// A scenario's workload.
pub(crate) struct Workload {
    pub(crate) id: &'static str,
    pub(crate) description: &'static str,
    pub(crate) deadline_s: u64,
    /// One call, no generated input and no other library work in the
    /// process: profilable.
    pub(crate) hotpath: bool,
    /// Whether rayon's global pool is started on the harness thread before
    /// setup. A scenario that leaves it unstarted may run no ambient call.
    pub(crate) starts_global_pool: bool,
    /// The soft `RLIMIT_NOFILE` policy, applied before any library work.
    pub(crate) nofile: Nofile,
    pub(crate) inputs: Vec<InputRole>,
    pub(crate) executor_pools: Vec<ExecutorPool>,
    pub(crate) fd_pools: Vec<DescriptorPool>,
    pub(crate) schedule: Schedule,
    /// The calls, in schedule order.
    pub(crate) calls: Vec<CallPlan>,
    pub(crate) assertions: Vec<AssertionDecl>,
}

/// An input role.
pub(crate) struct InputRole {
    pub(crate) id: &'static str,
    pub(crate) source: Source,
}

impl InputRole {
    pub(crate) const fn dataset(id: &'static str, variant: &'static str) -> Self {
        Self {
            id,
            source: Source::Dataset(variant),
        }
    }

    pub(crate) const fn generated(id: &'static str, recipe: Recipe) -> Self {
        Self {
            id,
            source: Source::Generated(recipe),
        }
    }
}

/// Where an input comes from.
pub(crate) enum Source {
    /// A dataset file of the run in the named variant.
    Dataset(&'static str),
    /// Written by setup before any call.
    Generated(Recipe),
}

/// How setup writes a generated input.
#[derive(Clone, Copy)]
pub(crate) enum Recipe {
    /// A sorted PBF of `blocks` blocks of [`NODES_PER_BLOCK`] untagged
    /// nodes, ids from 1, zlib:6, each block one blob with indexdata.
    Nodes { blocks: u64 },
    /// An empty file.
    EmptyFile,
}

impl Recipe {
    fn kind(self) -> GeneratedKind {
        match self {
            Self::Nodes { .. } => GeneratedKind::Pbf,
            Self::EmptyFile => GeneratedKind::File,
        }
    }

    fn to_json(self) -> Result<Value, String> {
        Ok(match self {
            Self::Nodes { blocks } => json!({
                "writer": "BlockBuilder and PbfWriter",
                "compression": compression_json(NODES_COMPRESSION)?,
                "header": if NODES_SORTED { "sorted" } else { "unsorted" },
                "blocks": blocks, "nodes_per_block": NODES_PER_BLOCK,
                "node_ids": "1 upward",
                "coordinates": format!("lat = lon = id * {NODES_COORD_STEP}"),
                "tags": "none", "metadata": "none",
            }),
            Self::EmptyFile => json!("an empty file"),
        })
    }

    /// Write the input at `path`.
    pub(crate) fn write(self, path: &Path) -> Result<(), String> {
        match self {
            Self::Nodes { blocks } => write_nodes(path, blocks).map_err(|e| e.to_string()),
            Self::EmptyFile => std::fs::write(path, b"").map_err(|e| e.to_string()),
        }
    }
}

/// How long a declared resource lives.
const LIFETIME: &str = "built before the first call, dropped once its last user has closed";

/// A rayon pool the runner builds before the first call and drops once the
/// last call naming it has closed, shared by every call that names it.
pub(crate) struct ExecutorPool {
    pub(crate) id: &'static str,
    pub(crate) threads: usize,
}

/// A reserving descriptor pool the runner builds before the first call and
/// drops once the last call naming it has closed, shared by every call that
/// names it.
pub(crate) struct DescriptorPool {
    pub(crate) id: &'static str,
    pub(crate) budget: u64,
}

/// How the calls run. The runner drives it from harness threads, never from
/// a worker of a pool a call uses.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Schedule {
    /// One after another, in declared order, each closed before the next.
    Sequential,
    /// Every call at once, each on a scoped harness thread of its own.
    Concurrent,
}

/// One call.
pub(crate) struct CallPlan {
    pub(crate) id: &'static str,
    pub(crate) label: &'static str,
    pub(crate) stream: &'static [&'static str],
    pub(crate) command: Command,
    pub(crate) host: HostPlan,
    /// What the scenario's handwritten observation does to the call (a tap
    /// that cancels, a descriptor rendezvous), for the plan. Unchecked.
    pub(crate) notes: Option<Value>,
}

impl CallPlan {
    /// A call on [`HostPlan::PLAIN`] claiming the sealed stream, no notes.
    pub(crate) fn plain(id: &'static str, label: &'static str, command: Command) -> Self {
        Self {
            id,
            label,
            stream: crate::protocol::STREAM_SEALED,
            command,
            host: HostPlan::PLAIN,
            notes: None,
        }
    }
}

/// The host a call gets.
pub(crate) struct HostPlan {
    pub(crate) parallelism: Parallelism,
    pub(crate) executor: ExecutorPlan,
    pub(crate) process: ProcessPlan,
    pub(crate) page_cache: PageCachePolicy,
}

impl HostPlan {
    /// Machine parallelism, the ambient pool, no process actions, the page
    /// cache kept.
    pub(crate) const PLAIN: Self = Self {
        parallelism: Parallelism::Machine,
        executor: ExecutorPlan::Ambient,
        process: ProcessPlan::None,
        page_cache: PageCachePolicy::Keep,
    };
}

#[derive(Clone, Copy)]
pub(crate) enum Parallelism {
    /// The machine's, read once at launch.
    Machine,
    Fixed(std::num::NonZeroUsize),
}

#[derive(Clone, Copy)]
pub(crate) enum ExecutorPlan {
    /// rayon's global pool.
    Ambient,
    /// The named [`ExecutorPool`].
    Pool(&'static str),
}

#[derive(Clone, Copy)]
pub(crate) enum ProcessPlan {
    /// `NoProcessActions`.
    None,
    /// The named [`DescriptorPool`].
    FdPool(&'static str),
}

/// A call's inputs.
pub(crate) enum Inputs {
    /// The paths bound to these roles.
    Roles(&'static [&'static str]),
    /// `count` raw spellings of [`MISSING`] under the private directory:
    /// `dir/name`, `dir/./name`, `dir/././name`, ... Each is its own pin
    /// (commands compare spellings byte for byte), so they are built as raw
    /// bytes, never normalised.
    MissingSpellings { count: usize },
}

impl Inputs {
    fn to_json(&self) -> Value {
        match self {
            Self::Roles(roles) => json!(roles),
            Self::MissingSpellings { count } => json!({
                "path": format!("{MISSING} in the call's private directory, never created"),
                "spellings": count,
                "spelling": "k ./ segments before the file name, k from 0",
            }),
        }
    }

    fn roles(&self) -> &'static [&'static str] {
        match self {
            Self::Roles(roles) => roles,
            Self::MissingSpellings { .. } => &[],
        }
    }

    /// Whether the inputs name [`MISSING`].
    pub(crate) fn names_missing(&self) -> bool {
        !matches!(self, Self::Roles(_))
    }

    fn paths(&self, env: &crate::runner::CallEnv<'_>) -> Vec<PathBuf> {
        let dir = env.private_dir();
        match self {
            Self::Roles(roles) => roles.iter().map(|r| env.binding(r).to_path_buf()).collect(),
            Self::MissingSpellings { count } => (0..*count)
                .map(|k| {
                    let mut path = OsString::from(dir.as_os_str());
                    path.push("/");
                    for _ in 0..k {
                        path.push("./");
                    }
                    path.push(MISSING);
                    PathBuf::from(path)
                })
                .collect(),
        }
    }
}

/// A single-input command's input. Only the missing file is spelled so
/// far; a role variant joins when a scenario reads one.
#[derive(Clone, Copy)]
pub(crate) enum OneInput {
    /// [`MISSING`] under the private directory, never created.
    Missing,
}

impl OneInput {
    fn to_json(self) -> Value {
        match self {
            Self::Missing => json!(format!(
                "{MISSING} in the call's private directory, never created"
            )),
        }
    }

    fn path(self, env: &crate::runner::CallEnv<'_>) -> PathBuf {
        match self {
            Self::Missing => env.private_dir().join(MISSING),
        }
    }
}

/// What a call's one output starts as.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Initial {
    Absent,
    /// Its seed (`brokkr-library-seed-v1 <role>` and a newline).
    Seed,
}

/// A call's output role and its policy. Staging is always
/// `DirWithBackups` in the private directory.
#[derive(Clone, Copy)]
pub(crate) struct OutputPlan {
    pub(crate) role: &'static str,
    pub(crate) overwrite: Overwrite,
    pub(crate) initial: Initial,
}

impl OutputPlan {
    /// A fresh output, refusing an existing destination.
    pub(crate) const fn fresh(role: &'static str) -> Self {
        Self {
            role,
            overwrite: Overwrite::Refuse,
            initial: Initial::Absent,
        }
    }
}

/// A library call, every option spelled.
pub(crate) enum Command {
    Cat {
        inputs: Inputs,
        output: OutputPlan,
        type_filter: Option<TypeFilter>,
        clean: CleanAttrs,
        write: WriteOptions,
        overrides: HeaderOverrides,
    },
    Digest {
        input: &'static str,
        form: Form,
        direct_io: bool,
    },
    /// One `extract_multi` over one ordered `OutputSet`, each region paired
    /// with its output in order.
    ExtractMulti {
        input: OneInput,
        /// The bbox text handed to `parse_bbox`, and its output role.
        regions: Vec<(&'static str, &'static str)>,
        overwrite: Overwrite,
        strategy: ExtractStrategy,
        set_bounds: bool,
        clean: CleanAttrs,
        write: WriteOptions,
        overrides: HeaderOverrides,
    },
    /// add-locations-to-ways with a run scratch in the private directory.
    Altw {
        input: &'static str,
        output: OutputPlan,
        options: AltwOptions,
        overrides: HeaderOverrides,
    },
}

/// What a command returned that judgments read.
#[derive(Clone, Copy)]
pub(crate) enum Returned {
    Cat { passthrough: u64, decoded: u64 },
    Digest(pbfhogg::digest::Digest),
    Nothing,
}

/// The write options every call uses unless it says otherwise, every field
/// spelled so a changed library default cannot change the workload unseen.
pub(crate) const WRITE: WriteOptions = WriteOptions {
    compression: Compression::Zlib(6),
    backend: OutputBackend::Buffered,
    direct_reads: false,
    force: false,
};

/// No attribute cleaned.
pub(crate) const CLEAN_NONE: CleanAttrs = CleanAttrs {
    version: false,
    changeset: false,
    timestamp: false,
    uid: false,
    user: false,
};

/// Every header field left to the command's policy.
pub(crate) fn no_overrides() -> HeaderOverrides {
    HeaderOverrides {
        generator: None,
        replication_timestamp: FieldOverride::Unchanged,
        replication_sequence_number: FieldOverride::Unchanged,
        replication_base_url: FieldOverride::Unchanged,
    }
}

/// A plain cat of `inputs` into a fresh `output`.
pub(crate) fn cat(inputs: &'static [&'static str], output: &'static str) -> Command {
    Command::Cat {
        inputs: Inputs::Roles(inputs),
        output: OutputPlan::fresh(output),
        type_filter: None,
        clean: CLEAN_NONE,
        write: WRITE,
        overrides: no_overrides(),
    }
}

impl Command {
    /// The output roles the command writes, in order.
    fn outputs(&self) -> Vec<(&'static str, Initial)> {
        match self {
            Self::Cat { output, .. } | Self::Altw { output, .. } => {
                vec![(output.role, output.initial)]
            }
            Self::Digest { .. } => Vec::new(),
            Self::ExtractMulti { regions, .. } => {
                regions.iter().map(|(_, r)| (*r, Initial::Absent)).collect()
            }
        }
    }

    /// The input roles the command reads.
    fn input_roles(&self) -> Vec<&'static str> {
        match self {
            Self::Cat { inputs, .. } => inputs.roles().to_vec(),
            Self::ExtractMulti { input, .. } => match input {
                OneInput::Missing => Vec::new(),
            },
            Self::Digest { input, .. } | Self::Altw { input, .. } => vec![*input],
        }
    }

    /// Whether the command names [`MISSING`].
    pub(crate) fn names_missing(&self) -> bool {
        match self {
            Self::Cat { inputs, .. } => inputs.names_missing(),
            Self::ExtractMulti { input, .. } => matches!(input, OneInput::Missing),
            Self::Digest { .. } | Self::Altw { .. } => false,
        }
    }

    /// The stance the context reports.
    pub(crate) fn overwrite(&self) -> Option<Overwrite> {
        match self {
            Self::Cat { output, .. } | Self::Altw { output, .. } => Some(output.overwrite),
            Self::ExtractMulti { overwrite, .. } => Some(*overwrite),
            Self::Digest { .. } => None,
        }
    }

    fn to_json(&self, host: &Value) -> Result<Value, String> {
        Ok(match self {
            Self::Cat {
                inputs,
                output,
                type_filter,
                clean,
                write,
                overrides,
            } => {
                let mut value = json!({
                    "command": "cat",
                    "inputs": inputs.to_json(),
                    "output": output.role,
                    "type_filter": type_filter.map(type_filter_json),
                    "clean": clean_json(*clean),
                    "write": write_json(*write)?,
                    "header_overrides": overrides_json(overrides),
                    "output_policy": output_policy_json(output.overwrite),
                    "host": host,
                });
                if output.initial == Initial::Seed {
                    value["output_initial"] = json!("seed");
                }
                value
            }
            Self::Digest {
                input,
                form,
                direct_io,
            } => json!({
                "command": "digest",
                "inputs": [input],
                "form": match form { Form::Content => "content", Form::Full => "full" },
                "direct_io": direct_io,
                "host": host,
            }),
            Self::ExtractMulti {
                input,
                regions,
                overwrite,
                strategy,
                set_bounds,
                clean,
                write,
                overrides,
            } => json!({
                "command": "extract",
                "input": input.to_json(),
                "regions": regions.iter()
                    .map(|(bbox, output)| json!({ "bbox": bbox, "output": output }))
                    .collect::<Vec<_>>(),
                "strategy": match strategy {
                    ExtractStrategy::Simple => "simple",
                    ExtractStrategy::CompleteWays => "complete_ways",
                    ExtractStrategy::Smart => "smart",
                },
                "set_bounds": set_bounds,
                "clean": clean_json(*clean),
                "write": write_json(*write)?,
                "header_overrides": overrides_json(overrides),
                "output_policy": output_policy_json(*overwrite),
                "host": host,
            }),
            Self::Altw {
                input,
                output,
                options,
                overrides,
            } => {
                let AltwOptions {
                    keep_untagged_nodes,
                    write,
                    index_type,
                    inject_prepass,
                    ignore_missing_nodes,
                } = *options;
                let mut value = json!({
                    "command": "add-locations-to-ways",
                    "input": input,
                    "output": output.role,
                    "index_type": match index_type {
                        IndexType::Sparse => "sparse",
                        IndexType::External => "external",
                        IndexType::Auto => "auto",
                    },
                    "keep_untagged_nodes": keep_untagged_nodes,
                    "inject_prepass": inject_prepass,
                    "ignore_missing_nodes": ignore_missing_nodes,
                    "write": write_json(write)?,
                    "header_overrides": overrides_json(overrides),
                    "output_policy": output_policy_json(output.overwrite),
                    "scratch": "a run scratch in the private directory",
                    "host": host,
                });
                if output.initial == Initial::Seed {
                    value["output_initial"] = json!("seed");
                }
                value
            }
        })
    }

    /// Run the command on `host`, building its outputs inside the call.
    pub(crate) fn execute(
        &self,
        env: &crate::runner::CallEnv<'_>,
        host: &Host,
    ) -> pbfhogg::Result<Returned> {
        match self {
            Self::Cat {
                inputs,
                output,
                type_filter,
                clean,
                write,
                overrides,
            } => {
                let out = build_output(env, output, host)?;
                let paths = inputs.paths(env);
                let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
                let stats =
                    pbfhogg::cat::cat(&refs, out, *type_filter, clean, write, overrides, host)?;
                Ok(Returned::Cat {
                    passthrough: stats.blobs_passthrough,
                    decoded: stats.blobs_decoded,
                })
            }
            Self::Digest {
                input,
                form,
                direct_io,
            } => pbfhogg::digest::digest(env.binding(input), *form, *direct_io, host)
                .map(Returned::Digest),
            Self::ExtractMulti {
                input,
                regions,
                overwrite,
                strategy,
                set_bounds,
                clean,
                write,
                overrides,
            } => {
                let path = input.path(env);
                let outputs = pbfhogg::OutputSet::files_with_staging(
                    regions.iter().map(|(_, role)| env.binding(role)),
                    *overwrite,
                    pbfhogg::Staging::DirWithBackups(env.private_dir()),
                    &host.diagnostics,
                )?;
                let mut parsed = Vec::with_capacity(regions.len());
                for (bbox, _) in regions {
                    parsed.push(pbfhogg::extract::Region::Bbox(
                        pbfhogg::extract::parse_bbox(bbox)?,
                    ));
                }
                pbfhogg::extract::extract_multi(
                    path.as_path(),
                    &parsed,
                    outputs,
                    *strategy,
                    *set_bounds,
                    clean,
                    write,
                    overrides,
                    host,
                )
                .map(|_| Returned::Nothing)
            }
            Self::Altw {
                input,
                output,
                options,
                overrides,
            } => {
                let out = build_output(env, output, host)?;
                let scratch = pbfhogg::scratch::RunScratch::create(env.private_dir(), host)?;
                pbfhogg::altw::add_locations_to_ways(
                    env.binding(input),
                    out,
                    options,
                    overrides,
                    host,
                    scratch,
                )
                .map(|_| Returned::Nothing)
            }
        }
    }
}

fn build_output(
    env: &crate::runner::CallEnv<'_>,
    output: &OutputPlan,
    host: &Host,
) -> pbfhogg::Result<pbfhogg::Output> {
    pbfhogg::Output::with_staging(
        env.binding(output.role),
        output.overwrite,
        pbfhogg::Staging::DirWithBackups(env.private_dir()),
        &host.diagnostics,
    )
}

fn compression_json(compression: Compression) -> Result<String, String> {
    Ok(match compression {
        Compression::None => "none".to_owned(),
        Compression::Zlib(level) => format!("zlib:{level}"),
        Compression::Zstd(level) => format!("zstd:{level}"),
        other => return Err(format!("no plan spelling for compression {other:?}")),
    })
}

fn write_json(write: WriteOptions) -> Result<Value, String> {
    let WriteOptions {
        compression,
        backend,
        direct_reads,
        force,
    } = write;
    Ok(json!({
        "compression": compression_json(compression)?,
        "backend": match backend {
            OutputBackend::Buffered => "buffered",
            OutputBackend::Parallel => "parallel",
            OutputBackend::Direct => "direct",
            OutputBackend::Uring => "uring",
        },
        "direct_reads": direct_reads,
        "force": force,
    }))
}

fn clean_json(clean: CleanAttrs) -> Value {
    let CleanAttrs {
        version,
        changeset,
        timestamp,
        uid,
        user,
    } = clean;
    json!({ "version": version, "changeset": changeset, "timestamp": timestamp,
            "uid": uid, "user": user })
}

fn type_filter_json(filter: TypeFilter) -> Value {
    let TypeFilter {
        nodes,
        ways,
        relations,
    } = filter;
    json!({ "node": nodes, "way": ways, "relation": relations })
}

fn field_json<T: Clone + Into<Value>>(field: &FieldOverride<T>) -> Value {
    match field {
        FieldOverride::Unchanged => json!("unchanged"),
        FieldOverride::Set(value) => json!({ "set": value.clone().into() }),
        FieldOverride::Clear => json!("clear"),
    }
}

fn overrides_json(overrides: &HeaderOverrides) -> Value {
    let HeaderOverrides {
        generator,
        replication_timestamp,
        replication_sequence_number,
        replication_base_url,
    } = overrides;
    json!({
        "generator": generator,
        "replication_timestamp": field_json(replication_timestamp),
        "replication_sequence_number": field_json(replication_sequence_number),
        "replication_base_url": field_json(replication_base_url),
    })
}

fn overwrite_name(overwrite: Overwrite) -> &'static str {
    match overwrite {
        Overwrite::Refuse => "refuse",
        Overwrite::Replace => "replace",
    }
}

fn output_policy_json(overwrite: Overwrite) -> Value {
    json!({ "overwrite": overwrite_name(overwrite), "staging": "dir_with_backups_private" })
}

/// The protocol's spelling of a page-cache policy, as plans and contexts
/// carry it.
pub(crate) fn page_cache_name(policy: PageCachePolicy) -> &'static str {
    match policy {
        PageCachePolicy::Keep => "keep",
        PageCachePolicy::Evict => "evict",
    }
}

/// The context's and plan's spelling of an output stance.
pub(crate) fn overwrite_json(overwrite: Option<Overwrite>) -> Value {
    overwrite.map_or(Value::Null, |o| json!(overwrite_name(o)))
}

impl HostPlan {
    fn to_json(&self) -> Value {
        let Self {
            parallelism,
            executor,
            process,
            page_cache,
        } = self;
        json!({
            "resources": "fixed",
            "parallelism": match parallelism {
                Parallelism::Machine => json!("machine"),
                Parallelism::Fixed(n) => json!(n.get()),
            },
            "memory_available": null,
            "process_actions": match process {
                ProcessPlan::None => json!("none"),
                ProcessPlan::FdPool(id) => json!({ "fd_pool": id }),
            },
            "page_cache": page_cache_name(*page_cache),
            "executor": match executor {
                ExecutorPlan::Ambient => json!("ambient"),
                ExecutorPlan::Pool(id) => json!({ "pool": id }),
            },
        })
    }
}

impl Workload {
    /// A sequential workload that starts the global pool and raises the
    /// soft descriptor limit, with nothing declared yet.
    pub(crate) fn new(id: &'static str, description: &'static str, deadline_s: u64) -> Self {
        Self {
            id,
            description,
            deadline_s,
            hotpath: false,
            starts_global_pool: true,
            nofile: Nofile::RaiseToHard,
            inputs: Vec::new(),
            executor_pools: Vec::new(),
            fd_pools: Vec::new(),
            schedule: Schedule::Sequential,
            calls: Vec::new(),
            assertions: Vec::new(),
        }
    }

    /// The calls naming a resource, in order.
    fn users(&self, uses: impl Fn(&HostPlan) -> bool) -> Vec<&'static str> {
        self.calls
            .iter()
            .filter(|c| uses(&c.host))
            .map(|c| c.id)
            .collect()
    }

    /// Index of the last call, in schedule order, naming a resource.
    pub(crate) fn last_user(&self, uses: impl Fn(&HostPlan) -> bool) -> Option<usize> {
        self.calls.iter().rposition(|c| uses(&c.host))
    }

    /// The plan: every workload choice, resolved, nothing run-specific.
    pub(crate) fn plan(&self) -> Result<Value, String> {
        let mut calls = serde_json::Map::new();
        for call in &self.calls {
            let mut value = call.command.to_json(&call.host.to_json())?;
            if let Some(notes) = &call.notes {
                value["notes"] = notes.clone();
            }
            calls.insert(call.id.to_owned(), value);
        }
        let mut generated = Vec::new();
        for input in &self.inputs {
            if let Source::Generated(recipe) = &input.source {
                generated.push(json!({ "role": input.id, "recipe": recipe.to_json()? }));
            }
        }
        let executor_pools: Vec<Value> = self
            .executor_pools
            .iter()
            .map(|p| {
                let users =
                    self.users(|h| matches!(h.executor, ExecutorPlan::Pool(id) if id == p.id));
                json!({ "id": p.id, "threads": p.threads, "shared_by": users, "lifetime": LIFETIME })
            })
            .collect();
        let fd_pools: Vec<Value> = self
            .fd_pools
            .iter()
            .map(|p| {
                let users =
                    self.users(|h| matches!(h.process, ProcessPlan::FdPool(id) if id == p.id));
                json!({ "id": p.id, "budget": p.budget, "shared_by": users, "lifetime": LIFETIME })
            })
            .collect();
        let order: Vec<&str> = self.calls.iter().map(|c| c.id).collect();
        Ok(json!({
            "schedule": {
                "order": order,
                "concurrency": match self.schedule {
                    Schedule::Sequential => "sequential",
                    Schedule::Concurrent => "all at once, one harness thread each",
                },
                "calls_driven_from": "harness threads, off every pool",
                "invocation": "one per call", "host": "one per call",
            },
            "setup": {
                "generated": generated,
                "rayon_global_pool": if self.starts_global_pool {
                    "started before setup"
                } else {
                    "never started"
                },
                "executor_pools": executor_pools,
                "fd_pools": fd_pools,
            },
            "process": {
                "rlimit_nofile_soft": self.nofile.to_json(),
                "sigpipe": "inherited",
            },
            "calls": calls,
        }))
    }

    /// The describe entry, without the assertions every scenario carries.
    pub(crate) fn decl(&self) -> ScenarioDecl {
        let mut roles: Vec<RoleDecl> = self
            .inputs
            .iter()
            .map(|i| RoleDecl {
                id: i.id,
                role: match &i.source {
                    Source::Dataset(variant) => Role::Dataset { variant },
                    Source::Generated(recipe) => Role::Generated {
                        kind: recipe.kind(),
                    },
                },
            })
            .collect();
        for call in &self.calls {
            for (role, initial) in call.command.outputs() {
                roles.push(RoleDecl {
                    id: role,
                    role: Role::OutputPbf {
                        owner: call.id,
                        seeded: initial == Initial::Seed,
                    },
                });
            }
        }
        ScenarioDecl {
            id: self.id,
            description: self.description,
            deadline_s: self.deadline_s,
            hotpath: self.hotpath,
            calls: self
                .calls
                .iter()
                .map(|c| CallDecl {
                    id: c.id,
                    label: c.label,
                    stream: c.stream,
                })
                .collect(),
            roles,
            assertions: self
                .assertions
                .iter()
                .map(|a| AssertionDecl {
                    id: a.id,
                    call: a.call,
                    covers: a.covers,
                })
                .collect(),
        }
    }

    /// The rules the declaration's own validation cannot see: every input
    /// role read by some call and every call's input declared, resources
    /// named exist and are used, no ambient call without the global pool,
    /// and the plan spelled.
    pub(crate) fn validate(&self) -> Result<(), String> {
        let bad = |why: String| Err(format!("{}: {why}", self.id));
        for call in &self.calls {
            for role in call.command.input_roles() {
                if !self.inputs.iter().any(|i| i.id == role) {
                    return bad(format!("call {} reads undeclared input {role}", call.id));
                }
            }
            if let ExecutorPlan::Pool(id) = call.host.executor
                && !self.executor_pools.iter().any(|p| p.id == id)
            {
                return bad(format!("call {} names no executor pool {id}", call.id));
            }
            if let ProcessPlan::FdPool(id) = call.host.process
                && !self.fd_pools.iter().any(|p| p.id == id)
            {
                return bad(format!("call {} names no descriptor pool {id}", call.id));
            }
            if matches!(call.host.executor, ExecutorPlan::Ambient) && !self.starts_global_pool {
                return bad(format!(
                    "call {} is ambient in a scenario that never starts the global pool",
                    call.id
                ));
            }
        }
        for input in &self.inputs {
            if !self
                .calls
                .iter()
                .any(|c| c.command.input_roles().contains(&input.id))
            {
                return bad(format!("input {} is read by no call", input.id));
            }
        }
        let mut ids = std::collections::BTreeSet::new();
        if !self.executor_pools.iter().all(|p| ids.insert(p.id)) {
            return bad("an executor pool id is declared twice".to_owned());
        }
        let mut ids = std::collections::BTreeSet::new();
        if !self.fd_pools.iter().all(|p| ids.insert(p.id)) {
            return bad("a descriptor pool id is declared twice".to_owned());
        }
        for pool in &self.executor_pools {
            if self
                .users(|h| matches!(h.executor, ExecutorPlan::Pool(id) if id == pool.id))
                .is_empty()
                || pool.threads == 0
            {
                return bad(format!("executor pool {} is unused or empty", pool.id));
            }
        }
        for pool in &self.fd_pools {
            if self
                .users(|h| matches!(h.process, ProcessPlan::FdPool(id) if id == pool.id))
                .is_empty()
            {
                return bad(format!("descriptor pool {} is unused", pool.id));
            }
        }
        if self.schedule == Schedule::Concurrent && self.calls.len() < 2 {
            return bad("a concurrent schedule of fewer than two calls".to_owned());
        }
        self.plan().map(drop)
    }
}

/// A sorted PBF of `blocks` blocks of untagged nodes, ids from 1, each
/// block written as one blob with indexdata.
fn write_nodes(path: &Path, blocks: u64) -> pbfhogg::Result<()> {
    use pbfhogg::block_builder::{BlockBuilder, HeaderBuilder};
    use pbfhogg::writer::PbfWriter;
    let file = std::fs::File::create(path)?;
    let mut writer = PbfWriter::new(std::io::BufWriter::new(file), NODES_COMPRESSION);
    let header = HeaderBuilder::new();
    let header = if NODES_SORTED {
        header.sorted()
    } else {
        header
    };
    writer.write_header(&header.build()?)?;
    let mut block = BlockBuilder::new();
    let mut id = 1_i64;
    for _ in 0..blocks {
        for _ in 0..NODES_PER_BLOCK {
            let lat = i32::try_from(id).unwrap_or(0) * NODES_COORD_STEP;
            block.add_node(id, lat, lat, std::iter::empty(), None);
            id += 1;
        }
        if let Some(bytes) = block.take()? {
            writer.write_primitive_block(bytes)?;
        }
    }
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::{
        CallPlan, DescriptorPool, ExecutorPlan, ExecutorPool, HostPlan, InputRole, ProcessPlan,
        Recipe, Schedule, Workload, cat,
    };

    fn one_cat() -> Workload {
        let mut w = Workload::new("w", "a workload", 60);
        w.inputs = vec![InputRole::dataset("input", "indexed")];
        w.calls = vec![CallPlan::plain("a", "cat", cat(&["input"], "out"))];
        w
    }

    #[test]
    fn a_plain_workload_validates() {
        one_cat().validate().expect("valid");
        let decl = one_cat().decl();
        decl.validate().expect("valid declaration");
        assert_eq!(decl.roles.len(), 2);
    }

    /// Each rule the workload carries refuses its own breach.
    #[test]
    fn workload_rules_refuse_their_breaches() {
        let mut ambient_unstarted = one_cat();
        ambient_unstarted.starts_global_pool = false;
        let mut unread = one_cat();
        unread.inputs.push(InputRole::dataset("other", "raw"));
        let mut undeclared = one_cat();
        undeclared.inputs.clear();
        let mut unknown_pool = one_cat();
        unknown_pool.calls[0].host.executor = ExecutorPlan::Pool("missing");
        let mut unknown_fd = one_cat();
        unknown_fd.calls[0].host.process = ProcessPlan::FdPool("missing");
        let mut unused_pool = one_cat();
        unused_pool.executor_pools.push(ExecutorPool {
            id: "idle",
            threads: 2,
        });
        let mut unused_fd = one_cat();
        unused_fd.fd_pools.push(DescriptorPool {
            id: "idle",
            budget: 4,
        });
        let mut lone_concurrent = one_cat();
        lone_concurrent.schedule = Schedule::Concurrent;
        let mut twice_fd = one_cat();
        twice_fd.calls[0].host.process = ProcessPlan::FdPool("p");
        twice_fd.fd_pools = vec![
            DescriptorPool {
                id: "p",
                budget: 18,
            },
            DescriptorPool { id: "p", budget: 9 },
        ];
        let mut twice_pool = one_cat();
        twice_pool.calls[0].host.executor = ExecutorPlan::Pool("p");
        twice_pool.executor_pools = vec![
            ExecutorPool {
                id: "p",
                threads: 2,
            },
            ExecutorPool {
                id: "p",
                threads: 3,
            },
        ];
        for (what, w) in [
            ("descriptor pool declared twice", twice_fd),
            ("executor pool declared twice", twice_pool),
            ("ambient call, global pool unstarted", ambient_unstarted),
            ("input read by no call", unread),
            ("call reads an undeclared input", undeclared),
            ("unknown executor pool", unknown_pool),
            ("unknown descriptor pool", unknown_fd),
            ("unused executor pool", unused_pool),
            ("unused descriptor pool", unused_fd),
            ("concurrent schedule of one call", lone_concurrent),
        ] {
            assert!(w.validate().is_err(), "{what}");
        }
    }

    /// A profilable workload may not generate an input: setup would do
    /// library work outside the declared call.
    #[test]
    fn hotpath_refuses_a_generated_input() {
        let mut w = Workload::new("w", "a workload", 60);
        w.hotpath = true;
        w.inputs = vec![InputRole::generated("input", Recipe::Nodes { blocks: 1 })];
        w.calls = vec![CallPlan::plain("a", "cat", cat(&["input"], "out"))];
        w.validate().expect("workload rules hold");
        assert!(w.decl().validate().is_err());
    }

    /// The host and its resources are part of the plan: changing either
    /// changes the call's plan.
    #[test]
    fn the_plan_carries_the_host() {
        let base = one_cat().plan().expect("plan");
        let mut evicting = one_cat();
        evicting.calls[0].host = HostPlan {
            page_cache: pbfhogg::host::PageCachePolicy::Evict,
            ..HostPlan::PLAIN
        };
        assert_ne!(base, evicting.plan().expect("plan"));
        let mut pooled = one_cat();
        pooled.executor_pools.push(ExecutorPool {
            id: "p",
            threads: 2,
        });
        pooled.calls[0].host.executor = ExecutorPlan::Pool("p");
        let pooled = pooled.plan().expect("plan");
        assert_eq!(pooled["setup"]["executor_pools"][0]["shared_by"][0], "a");
        assert_ne!(base, pooled);
    }
}
