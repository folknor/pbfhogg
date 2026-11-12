//! The cancellation contract at the library level (ADR-0019).
//!
//! A command run under a `Host` whose `Invocation` token is tripped - before the
//! call, or at any checkpoint during it - returns `ErrorKind::Cancelled`,
//! publishes nothing, and leaves no output staging entry and no run scratch
//! behind. A publication whose decision was taken before the token tripped
//! completes, and its own outcome (an unsynced directory, say) stays
//! authoritative. Tokens are per invocation: tripping one stops only the runs
//! that observe it.
//!
//! A registry gives every public library command module fixture scenarios
//! (and states the modes it leaves out), checked against the module
//! declarations; each module gets its own sweep tests. The mid-run sweep
//! trips the token from the checkpoint hook at sampled checkpoint ordinals
//! of each scenario, in process, so "mid-run" needs no signal and no timing.
//! Module completeness is not entry or mode completeness: the registry
//! names what each module's scenarios leave out.

#![allow(clippy::unwrap_used, clippy::unwrap_in_result)]

use std::path::{Path, PathBuf};

use pbfhogg_test_scratch::TestScratch;

/// One library command, run against fixtures in `inputs`, writing to `out`
/// and putting any scratch in `scratch`.
type Command = fn(&Inputs, &Path, &Path, &pbfhogg::host::Host) -> pbfhogg::Result<()>;

struct Inputs {
    dir: TestScratch,
    /// The heavy ticket a scenario that needs one (an external join) runs
    /// under, one run at a time; `None` builds light hosts.
    ticket: Option<std::sync::Arc<crate::common::FdTicket>>,
}

impl Inputs {
    /// Inputs for light scenarios.
    fn new() -> Self {
        Self::build(None)
    }

    /// Inputs for a heavy scenario, or `None` when its ticket was not
    /// admitted (reported as a skip).
    fn heavy(test: &str) -> Option<Self> {
        crate::common::heavy_ticket(1, test).map(|ticket| Self::build(Some(ticket)))
    }

    fn build(ticket: Option<std::sync::Arc<crate::common::FdTicket>>) -> Self {
        let dir = crate::common::test_scratch();
        crate::common::write_base_pbf(&dir.path().join("base.osm.pbf"));
        // The base with LocationsOnWays, for the commands that need way
        // geometry (export's ways, check's locations).
        let host = crate::common::test_host();
        pbfhogg::altw::add_locations_to_ways(
            &dir.path().join("base.osm.pbf"),
            crate::common::out(dir.path().join("altw.osm.pbf")),
            &pbfhogg::altw::AltwOptions {
                keep_untagged_nodes: true,
                write: Default::default(),
                index_type: pbfhogg::altw::IndexType::Sparse,
                inject_prepass: false,
                ignore_missing_nodes: false,
            },
            &Default::default(),
            &host,
            pbfhogg::scratch::RunScratch::create(dir.path(), &host).unwrap(),
        )
        .unwrap();
        crate::common::write_changed_pbf(&dir.path().join("changed.osm.pbf"));
        // Elements with metadata, which time-filter refuses to go without.
        let mut nodes = crate::common::generate_nodes(300, 1);
        let mut ways = crate::common::generate_ways(40, 1, 5, 1);
        for n in &mut nodes {
            n.meta = Some(Default::default());
        }
        for w in &mut ways {
            w.meta = Some(Default::default());
        }
        crate::common::write_multi_block_test_pbf(
            &dir.path().join("meta.osm.pbf"),
            &nodes,
            &ways,
            &[],
            100,
        );
        #[cfg(feature = "commands")]
        crate::common::write_synthetic_indexed_input(&dir.path().join("geo.osm.pbf"));
        std::fs::write(
            dir.path().join("change.osc"),
            b"<osmChange version=\"0.6\"><create>\
              <node id=\"1000\" version=\"1\" lat=\"0.001\" lon=\"0.002\">\
              <tag k=\"amenity\" v=\"cafe\"/></node></create></osmChange>",
        )
        .unwrap();
        Self { dir, ticket }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
}

fn host_with(cancel: pbfhogg::host::Invocation, inputs: &Inputs) -> pbfhogg::host::Host {
    let host = match &inputs.ticket {
        Some(ticket) => crate::common::heavy_host(ticket),
        None => crate::common::test_host(),
    };
    pbfhogg::host::Host {
        invocation: cancel,
        ..host
    }
}

fn sort(i: &Inputs, out: &Path, _: &Path, host: &pbfhogg::host::Host) -> pbfhogg::Result<()> {
    pbfhogg::sort::sort(
        &i.path("base.osm.pbf"),
        crate::common::out(out.join("out.osm.pbf")),
        &Default::default(),
        &Default::default(),
        host,
    )
    .map(drop)
}

fn cat(i: &Inputs, out: &Path, _: &Path, host: &pbfhogg::host::Host) -> pbfhogg::Result<()> {
    let base = i.path("base.osm.pbf");
    pbfhogg::cat::cat(
        &[&base],
        crate::common::out(out.join("out.osm.pbf")),
        Some(pbfhogg::TypeFilter::all()),
        &Default::default(),
        &Default::default(),
        &Default::default(),
        host,
    )
    .map(drop)
}

fn altw(
    i: &Inputs,
    out: &Path,
    scratch: &Path,
    host: &pbfhogg::host::Host,
    index_type: pbfhogg::altw::IndexType,
) -> pbfhogg::Result<()> {
    pbfhogg::altw::add_locations_to_ways(
        &i.path("base.osm.pbf"),
        crate::common::out(out.join("out.osm.pbf")),
        &pbfhogg::altw::AltwOptions {
            keep_untagged_nodes: true,
            write: Default::default(),
            index_type,
            inject_prepass: false,
            ignore_missing_nodes: false,
        },
        &Default::default(),
        host,
        pbfhogg::scratch::RunScratch::create(scratch, host)?,
    )
    .map(drop)
}

fn altw_sparse(
    i: &Inputs,
    out: &Path,
    scratch: &Path,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<()> {
    altw(i, out, scratch, host, pbfhogg::altw::IndexType::Sparse)
}

fn altw_external(
    i: &Inputs,
    out: &Path,
    scratch: &Path,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<()> {
    altw(i, out, scratch, host, pbfhogg::altw::IndexType::External)
}

fn apply_changes(
    i: &Inputs,
    out: &Path,
    _: &Path,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<()> {
    pbfhogg::apply_changes::merge(
        &i.path("base.osm.pbf"),
        &i.path("change.osc"),
        crate::common::out(out.join("out.osm.pbf")),
        &pbfhogg::apply_changes::MergeOptions::default(),
        &Default::default(),
        host,
    )
    .map(drop)
}

fn merge_changes(
    i: &Inputs,
    out: &Path,
    scratch: &Path,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<()> {
    let osc = i.path("change.osc");
    // The second input spills its one member, and its file is closed,
    // before the first is read: every checkpoint of the run then falls at a
    // fixed point of the spill, so the sweep trips inside it and inside the
    // copy-back.
    #[cfg(pbfhogg_test_hooks)]
    host.invocation
        .faults()
        .merge_changes_spill_first
        .store(1, std::sync::atomic::Ordering::SeqCst);
    pbfhogg::merge_changes::merge_changes(
        &[&osc, &osc],
        crate::common::out(out.join("merged.osc.gz")),
        pbfhogg::merge_changes::MergeMode::Stream {
            scratch: Some(pbfhogg::scratch::RunScratch::create(scratch, host)?),
        },
        pbfhogg::Parallelism::from_jobs(2),
        host,
    )
    .map(drop)
}

fn diff_parallel(
    i: &Inputs,
    out: &Path,
    scratch: &Path,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<()> {
    // A text diff writes neither a file output nor scratch.
    let _ = (out, scratch);
    pbfhogg::diff::diff(
        &i.path("base.osm.pbf"),
        &i.path("changed.osm.pbf"),
        &mut Vec::new(),
        &pbfhogg::diff::DiffOptions {
            suppress_common: false,
            verbose: false,
            type_filter: None,
            parallelism: pbfhogg::Parallelism::from_jobs(2),
        },
        false,
        host,
    )
    .map(drop)
}

fn derive_changes(
    i: &Inputs,
    out: &Path,
    scratch: &Path,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<()> {
    pbfhogg::diff::derive::derive_changes(
        &i.path("base.osm.pbf"),
        &i.path("changed.osm.pbf"),
        crate::common::out(out.join("delta.osc.gz")),
        false,
        false,
        None,
        pbfhogg::Parallelism::from_jobs(2),
        host,
        pbfhogg::scratch::RunScratch::create(scratch, host)?,
    )
    .map(drop)
}

#[cfg(feature = "commands")]
fn geocode(
    i: &Inputs,
    out: &Path,
    scratch: &Path,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<()> {
    let output = pbfhogg::geocode_index::builder::index_output(
        out.join("index"),
        pbfhogg::Overwrite::Refuse,
        &host.diagnostics,
    )?;
    pbfhogg::geocode_index::builder::build_geocode_index(
        &i.path("geo.osm.pbf"),
        &pbfhogg::geocode_index::builder::BuildConfig {
            allow_unindexed: true,
            ..Default::default()
        },
        output,
        host,
        pbfhogg::scratch::RunScratch::create(scratch, host)?,
    )
    .map(drop)
}

fn digest(i: &Inputs, _: &Path, _: &Path, host: &pbfhogg::host::Host) -> pbfhogg::Result<()> {
    pbfhogg::digest::digest(
        &i.path("base.osm.pbf"),
        pbfhogg::digest::Form::Full,
        false,
        host,
    )
    .map(drop)
}

#[cfg(feature = "commands")]
fn check_refs(i: &Inputs, _: &Path, _: &Path, host: &pbfhogg::host::Host) -> pbfhogg::Result<()> {
    pbfhogg::check::refs::check_refs(
        &i.path("base.osm.pbf"),
        &pbfhogg::check::refs::CheckRefsOptions {
            direct_io: false,
            check_relations: true,
            show_ids: None,
        },
        host,
    )
    .map(drop)
}

#[cfg(feature = "commands")]
fn check_index(i: &Inputs, _: &Path, _: &Path, host: &pbfhogg::host::Host) -> pbfhogg::Result<()> {
    pbfhogg::check::index::check_index(
        &i.path("base.osm.pbf"),
        &pbfhogg::check::index::CheckIndexOptions {
            max_errors: 0,
            direct_io: false,
        },
        host,
    )
    .map(drop)
}

#[cfg(feature = "commands")]
fn check_ids(i: &Inputs, _: &Path, _: &Path, host: &pbfhogg::host::Host) -> pbfhogg::Result<()> {
    pbfhogg::check::verify_ids::verify_ids(
        &i.path("base.osm.pbf"),
        &pbfhogg::check::verify_ids::VerifyIdsOptions {
            max_errors: 0,
            direct_io: false,
            full: true,
            type_filter: None,
        },
        host,
    )
    .map(drop)
}

#[cfg(feature = "commands")]
fn check_locations(
    i: &Inputs,
    _: &Path,
    _: &Path,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<()> {
    pbfhogg::check::locations::check_locations(
        &i.path("altw.osm.pbf"),
        &pbfhogg::check::locations::CheckLocationsOptions {
            max_errors: 0,
            direct_io: false,
            agreement: false,
            store_budget_bytes: None,
        },
        host,
    )
    .map(drop)
}

fn degrade(i: &Inputs, out: &Path, _: &Path, host: &pbfhogg::host::Host) -> pbfhogg::Result<()> {
    pbfhogg::degrade::degrade(
        &i.path("base.osm.pbf"),
        crate::common::out(out.join("out.osm.pbf")),
        pbfhogg::degrade::DegradeFlags {
            strip_indexdata: true,
            ..Default::default()
        },
        &Default::default(),
        &Default::default(),
        host,
    )
    .map(drop)
}

#[cfg(feature = "commands")]
fn export(i: &Inputs, _: &Path, _: &Path, host: &pbfhogg::host::Host) -> pbfhogg::Result<()> {
    pbfhogg::export::export(
        &i.path("altw.osm.pbf"),
        Vec::new(),
        &pbfhogg::export::ExportOptions::new(
            Default::default(),
            Default::default(),
            &[],
            None,
            None,
            false,
        )?,
        host,
    )
    .map(drop)
}

#[cfg(feature = "commands")]
fn extract(
    i: &Inputs,
    out: &Path,
    host: &pbfhogg::host::Host,
    strategy: pbfhogg::extract::ExtractStrategy,
) -> pbfhogg::Result<()> {
    pbfhogg::extract::extract(
        &i.path("base.osm.pbf"),
        crate::common::out(out.join("out.osm.pbf")),
        &pbfhogg::extract::Region::Bbox(pbfhogg::extract::parse_bbox("-180,-90,180,90").unwrap()),
        strategy,
        true,
        &Default::default(),
        &Default::default(),
        &Default::default(),
        host,
    )
    .map(drop)
}

#[cfg(feature = "commands")]
fn extract_simple(
    i: &Inputs,
    out: &Path,
    _: &Path,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<()> {
    extract(i, out, host, pbfhogg::extract::ExtractStrategy::Simple)
}

#[cfg(feature = "commands")]
fn extract_smart(
    i: &Inputs,
    out: &Path,
    _: &Path,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<()> {
    extract(i, out, host, pbfhogg::extract::ExtractStrategy::Smart)
}

fn ids(specs: &[&str]) -> pbfhogg::getid::ElementIds {
    let specs: Vec<String> = specs.iter().map(ToString::to_string).collect();
    pbfhogg::getid::parse_ids(&specs).unwrap()
}

fn getid(i: &Inputs, out: &Path, _: &Path, host: &pbfhogg::host::Host) -> pbfhogg::Result<()> {
    pbfhogg::getid::getid(
        &i.path("base.osm.pbf"),
        crate::common::out(out.join("out.osm.pbf")),
        &ids(&["n1", "w1", "r1"]),
        &pbfhogg::getid::GetidOptions {
            add_referenced: None,
            report_found: false,
        },
        &Default::default(),
        &Default::default(),
        host,
    )
    .map(drop)
}

fn removeid(i: &Inputs, out: &Path, _: &Path, host: &pbfhogg::host::Host) -> pbfhogg::Result<()> {
    pbfhogg::getid::removeid(
        &i.path("base.osm.pbf"),
        crate::common::out(out.join("out.osm.pbf")),
        &ids(&["n1", "w1", "r1"]),
        &Default::default(),
        &Default::default(),
        host,
    )
    .map(drop)
}

fn getparents(i: &Inputs, out: &Path, _: &Path, host: &pbfhogg::host::Host) -> pbfhogg::Result<()> {
    pbfhogg::getparents::getparents(
        &i.path("base.osm.pbf"),
        crate::common::out(out.join("out.osm.pbf")),
        &ids(&["n1", "w1"]),
        &pbfhogg::getparents::GetparentsOptions { add_self: true },
        &Default::default(),
        &Default::default(),
        host,
    )
    .map(drop)
}

fn inspect_with(i: &Inputs, host: &pbfhogg::host::Host, extended: bool) -> pbfhogg::Result<()> {
    pbfhogg::inspect::inspect(
        &i.path("base.osm.pbf"),
        &pbfhogg::inspect::InspectOptions {
            show_blocks: true,
            show_id_ranges: true,
            show_locations: false,
            extended,
            distrust_metadata: false,
            direct_io: false,
        },
        host,
    )
    .map(drop)
}

fn inspect_indexed(
    i: &Inputs,
    _: &Path,
    _: &Path,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<()> {
    inspect_with(i, host, false)
}

fn inspect_extended(
    i: &Inputs,
    _: &Path,
    _: &Path,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<()> {
    inspect_with(i, host, true)
}

fn inspect_header(
    i: &Inputs,
    _: &Path,
    _: &Path,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<()> {
    pbfhogg::inspect::inspect_header(&i.path("base.osm.pbf"), false, host).map(drop)
}

fn node_stats(i: &Inputs, _: &Path, _: &Path, host: &pbfhogg::host::Host) -> pbfhogg::Result<()> {
    pbfhogg::inspect::node_stats::node_stats(
        &i.path("base.osm.pbf"),
        false,
        false,
        pbfhogg::Parallelism::from_jobs(2),
        host,
    )
    .map(drop)
}

fn show_element(i: &Inputs, _: &Path, _: &Path, host: &pbfhogg::host::Host) -> pbfhogg::Result<()> {
    pbfhogg::inspect::show_element_to(
        &i.path("base.osm.pbf"),
        pbfhogg::ElementKind::Way,
        20,
        false,
        &mut Vec::new(),
        host,
    )
    .map(drop)
}

fn renumber(i: &Inputs, out: &Path, _: &Path, host: &pbfhogg::host::Host) -> pbfhogg::Result<()> {
    pbfhogg::renumber::renumber_external(
        &i.path("base.osm.pbf"),
        crate::common::out(out.join("out.osm.pbf")),
        &pbfhogg::renumber::RenumberOptions {
            start_node_id: 1,
            start_way_id: 1,
            start_relation_id: 1,
        },
        &Default::default(),
        &Default::default(),
        host,
    )
    .map(drop)
}

fn repack(i: &Inputs, out: &Path, _: &Path, host: &pbfhogg::host::Host) -> pbfhogg::Result<()> {
    pbfhogg::repack::repack(
        &i.path("base.osm.pbf"),
        crate::common::out(out.join("out.osm.pbf")),
        std::num::NonZeroUsize::new(50).unwrap(),
        &Default::default(),
        &Default::default(),
        host,
    )
    .map(drop)
}

fn tags_count(i: &Inputs, _: &Path, _: &Path, host: &pbfhogg::host::Host) -> pbfhogg::Result<()> {
    pbfhogg::tags_count::tags_count(
        &i.path("base.osm.pbf"),
        &pbfhogg::tags_count::TagCountOptions {
            min_count: 1,
            max_count: None,
            sort: Default::default(),
            expressions: &[],
            type_filter: None,
            direct_io: false,
            force: false,
            parallelism: pbfhogg::Parallelism::from_jobs(2),
        },
        host,
    )
    .map(drop)
}

fn tags_filter(
    i: &Inputs,
    out: &Path,
    host: &pbfhogg::host::Host,
    add_referenced: Option<pbfhogg::tags_filter::AddReferenced>,
) -> pbfhogg::Result<()> {
    let expressions = ["w/highway".to_owned(), "n/amenity".to_owned()];
    pbfhogg::tags_filter::tags_filter(
        &i.path("base.osm.pbf"),
        crate::common::out(out.join("out.osm.pbf")),
        &pbfhogg::tags_filter::TagsFilterOptions {
            expression_strs: &expressions,
            add_referenced,
            invert: false,
            write: Default::default(),
            distrust_metadata: false,
        },
        &Default::default(),
        host,
    )
    .map(drop)
}

fn tags_filter_direct(
    i: &Inputs,
    out: &Path,
    _: &Path,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<()> {
    tags_filter(i, out, host, None)
}

fn tags_filter_referenced(
    i: &Inputs,
    out: &Path,
    _: &Path,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<()> {
    tags_filter(
        i,
        out,
        host,
        Some(pbfhogg::tags_filter::AddReferenced {
            remove_tags: false,
            parallelism: pbfhogg::Parallelism::from_jobs(2),
        }),
    )
}

fn tags_filter_osc(
    i: &Inputs,
    out: &Path,
    _: &Path,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<()> {
    pbfhogg::tags_filter::osc::tags_filter_osc(
        &i.path("change.osc"),
        crate::common::out(out.join("out.osc")),
        &["n/amenity".to_owned()],
        pbfhogg::Parallelism::from_jobs(2),
        host,
    )
    .map(drop)
}

fn time_filter(
    i: &Inputs,
    out: &Path,
    _: &Path,
    host: &pbfhogg::host::Host,
) -> pbfhogg::Result<()> {
    pbfhogg::time_filter::time_filter(
        &i.path("meta.osm.pbf"),
        crate::common::out(out.join("out.osm.pbf")),
        i64::MAX,
        &Default::default(),
        &Default::default(),
        host,
    )
    .map(drop)
}

/// Inventory ownership is by public library command module, not CLI spelling.
/// Library tests cannot depend on pbfhogg-cli (which depends on this library).
/// Modes are declared here rather than inferred from scenario display names.
struct CommandCoverage {
    module: &'static str,
    scenarios: Vec<Scenario>,
    excluded_modes: &'static str,
}

struct Scenario {
    mode: &'static str,
    run: Command,
    /// Runs under a heavy descriptor ticket of its own (an external join).
    heavy: bool,
}

fn light(mode: &'static str, run: Command) -> Scenario {
    Scenario {
        mode,
        run,
        heavy: false,
    }
}

fn tested(
    module: &'static str,
    scenarios: Vec<Scenario>,
    excluded_modes: &'static str,
) -> CommandCoverage {
    CommandCoverage {
        module,
        scenarios,
        excluded_modes,
    }
}

// check, export and extract compile only with `commands`; without it their
// modules still count in the inventory and simply have nothing to run.

#[cfg(feature = "commands")]
fn check_scenarios() -> Vec<Scenario> {
    vec![
        light("refs, relation members", check_refs),
        light("index against payloads", check_index),
        light("IDs, full duplicate sets", check_ids),
        light("locations, LocationsOnWays input", check_locations),
    ]
}

#[cfg(feature = "commands")]
fn export_scenarios() -> Vec<Scenario> {
    vec![light(
        "GeoJSON sequence, all types, LocationsOnWays input",
        export,
    )]
}

#[cfg(feature = "commands")]
fn extract_scenarios() -> Vec<Scenario> {
    vec![
        light("single bbox, simple strategy", extract_simple),
        light("single bbox, smart strategy", extract_smart),
    ]
}

#[cfg(not(feature = "commands"))]
fn check_scenarios() -> Vec<Scenario> {
    Vec::new()
}

#[cfg(not(feature = "commands"))]
fn export_scenarios() -> Vec<Scenario> {
    Vec::new()
}

#[cfg(not(feature = "commands"))]
fn extract_scenarios() -> Vec<Scenario> {
    Vec::new()
}

fn command_registry() -> Vec<CommandCoverage> {
    let mut registry = registry_head();
    registry.extend(registry_tail());
    registry
}

fn registry_head() -> Vec<CommandCoverage> {
    vec![
        tested(
            "altw",
            vec![
                light("sparse, keep untagged, no prepass", altw_sparse),
                Scenario {
                    mode: "external, keep untagged, no prepass",
                    run: altw_external,
                    heavy: true,
                },
            ],
            "Auto routing, inject prepass, missing-node tolerance and dropping untagged nodes are not exercised.",
        ),
        tested(
            "apply_changes",
            vec![light("default PBF base plus XML OSC", apply_changes)],
            "LocationsOnWays and nondefault merge/write options are not exercised.",
        ),
        tested(
            "cat",
            vec![light(
                "single input, explicit all-type filter, default write",
                cat,
            )],
            "Multiple inputs, dedupe, cleaning and individual type filters are not exercised.",
        ),
        tested(
            "check",
            check_scenarios(),
            "Shown missing-reference IDs, violation caps, location agreement and store budgets are not exercised.",
        ),
        tested(
            "degrade",
            vec![light("strip indexdata, header-only passthrough", degrade)],
            "Reencoding (unsort, strip locations, drop IDs) and the tagdata edits are not exercised.",
        ),
        tested(
            "diff",
            vec![
                light(
                    "text diff, two jobs, include common, no type filter",
                    diff_parallel,
                ),
                light(
                    "derive XML gzip changes, two jobs, default flags",
                    derive_changes,
                ),
            ],
            "Serial execution, suppressed common, verbose, type filtering and other derive options are not exercised.",
        ),
        tested(
            "digest",
            vec![light("full form, default flags", digest)],
            "Other forms and nondefault flags are not exercised.",
        ),
    ]
}

fn registry_tail() -> Vec<CommandCoverage> {
    vec![
        tested(
            "export",
            export_scenarios(),
            "Plain GeoJSON, type, expression, property and bbox selections and metadata are not exercised.",
        ),
        tested(
            "extract",
            extract_scenarios(),
            "Polygons, complete-ways, multi-extract and cleaning are not exercised.",
        ),
        tested(
            "getid",
            vec![
                light("getid, exact IDs, no reference completion", getid),
                light("removeid, exact IDs", removeid),
            ],
            "Reference completion, found-ID reporting and ID lists read from a PBF are not exercised.",
        ),
        tested(
            "getparents",
            vec![light("node and way IDs, add self", getparents)],
            "Parents without the queried objects and the minimum-blob variant are not exercised.",
        ),
        tested(
            "inspect",
            vec![
                light("indexdata scan, blocks and ID ranges", inspect_indexed),
                light("extended full decode", inspect_extended),
                light("header only", inspect_header),
                light("node statistics, two jobs", node_stats),
                light("show one way", show_element),
            ],
            "Location diagnostics, distrusted metadata, direct reads and the get-key lookups are not exercised.",
        ),
        tested(
            "merge_changes",
            vec![light(
                "stream, two jobs, two XML inputs, forced spill with test-hooks",
                merge_changes,
            )],
            "In-memory merge, other job counts and compression/input variants are not exercised.",
        ),
        tested(
            "renumber",
            vec![light("external, IDs from one", renumber)],
            "Other start IDs and nondefault write options are not exercised.",
        ),
        tested(
            "repack",
            vec![light("50 elements per blob", repack)],
            "Nondefault write options are not exercised.",
        ),
        tested(
            "sort",
            vec![light("default options on sorted fixture", sort)],
            "Forced sorting, external runs and nondefault sort/write options are not exercised.",
        ),
        tested(
            "tags_count",
            vec![light("every key, two jobs", tags_count)],
            "Expressions, type filters, count bounds, sort orders and forced decode are not exercised.",
        ),
        tested(
            "tags_filter",
            vec![
                light("PBF, direct matches only", tags_filter_direct),
                light("PBF, add referenced, two jobs", tags_filter_referenced),
                light("XML OSC input, two jobs", tags_filter_osc),
            ],
            "Inversion, tag removal and distrusted metadata are not exercised.",
        ),
        tested(
            "time_filter",
            vec![light("cutoff after every timestamp", time_filter)],
            "Cutoffs that drop versions and nondefault write options are not exercised.",
        ),
    ]
}

/// An added public command module must acquire either scenarios or an explicit
/// exclusion. Read all declarations, including feature-gated ones, so consumer
/// builds enforce the same inventory as commands-enabled builds. Private helper
/// modules are deliberately outside this command inventory.
#[test]
fn cancellation_registry_accounts_for_public_library_command_modules() {
    use std::collections::BTreeSet;

    let inventory: BTreeSet<_> = include_str!("../src/commands/mod.rs")
        .lines()
        .filter_map(|line| line.trim().strip_prefix("pub mod "))
        .map(|declaration| {
            declaration
                .strip_suffix(';')
                .expect("public module declaration shape")
        })
        .collect();
    assert!(!inventory.is_empty(), "command inventory must not be empty");
    let registry = command_registry();
    let registered: BTreeSet<_> = registry.iter().map(|entry| entry.module).collect();
    assert_eq!(registered.len(), registry.len(), "duplicate command module");
    assert_eq!(
        inventory, registered,
        "unaccounted or removed command module"
    );
    let swept: BTreeSet<_> = SWEPT
        .iter()
        .copied()
        .filter(|module| *module != "geocode_builder")
        .collect();
    assert_eq!(swept.len() + 1, SWEPT.len(), "duplicate swept module");
    assert_eq!(
        swept, registered,
        "a registered module without its sweep tests"
    );
    for entry in registry {
        // Every module has a scenario; only check, export and extract go
        // without one, in builds without `commands`, where they do not exist.
        assert!(
            !entry.scenarios.is_empty() || cfg!(not(feature = "commands")),
            "{}: no scenario",
            entry.module
        );
        assert!(
            !entry.excluded_modes.trim().is_empty(),
            "{}: declare exclusions",
            entry.module
        );
        let modes: BTreeSet<_> = entry
            .scenarios
            .iter()
            .map(|scenario| scenario.mode)
            .collect();
        assert_eq!(
            modes.len(),
            entry.scenarios.len(),
            "{}: duplicate mode",
            entry.module
        );
        assert!(modes.iter().all(|mode| !mode.trim().is_empty()));
    }
}

/// The scenarios of one swept module. Geocode's builder lives outside
/// commands/, so it is swept as `geocode_builder`, not a command-module
/// inventory member. Its single indexed-fixture build permits unindexed
/// input and excludes unindexed fixtures and nondefault build policies. No
/// geocode scenario runs without `commands`.
fn scenarios(module: &str) -> Vec<Scenario> {
    if module == "geocode_builder" {
        #[cfg(feature = "commands")]
        return vec![light(
            "indexed fixture, allow unindexed, default policies",
            geocode,
        )];
        #[cfg(not(feature = "commands"))]
        return Vec::new();
    }
    command_registry()
        .into_iter()
        .find(|entry| entry.module == module)
        .map(|entry| entry.scenarios)
        .unwrap_or_else(|| panic!("{module} is not in the command registry"))
}

/// Each swept module gets its own three sweep tests, so a test's time is
/// one module's scenarios, not the whole registry's, under the fixed
/// per-test watchdog. `SWEPT` is checked against the registry.
macro_rules! sweeps {
    ($($module:ident),* $(,)?) => {
        const SWEPT: &[&str] = &[$(stringify!($module)),*];
        $(
            mod $module {
                #[test]
                fn a_tripped_token_stops_it_before_it_starts() {
                    super::sweep_before_start(module_path!(), stringify!($module));
                }

                #[cfg(pbfhogg_test_hooks)]
                #[test]
                fn a_token_tripped_at_sampled_ordinals_cancels_or_follows_a_decision() {
                    super::sweep_sampled_ordinals(module_path!(), stringify!($module));
                }

                #[cfg(pbfhogg_test_hooks)]
                #[test]
                fn the_census_is_empty_at_observed_checkpoints() {
                    super::sweep_census(module_path!(), stringify!($module));
                }
            }
        )*
    };
}

sweeps!(
    altw,
    apply_changes,
    cat,
    check,
    degrade,
    diff,
    digest,
    export,
    extract,
    getid,
    getparents,
    inspect,
    merge_changes,
    renumber,
    repack,
    sort,
    tags_count,
    tags_filter,
    time_filter,
    geocode_builder,
);

/// Run `each` over the scenarios of `module`, named `module: mode`. Light
/// scenarios share one set of inputs; a heavy one takes a ticket of its
/// own and is skipped (environmentally) when not admitted. Collects every
/// failure and fails the test once with all of them.
fn sweep(test: &str, module: &str, mut each: impl FnMut(&str, Command, &Inputs) -> Vec<String>) {
    let light = Inputs::new();
    let mut failures = Vec::new();
    for scenario in scenarios(module) {
        let name = format!("{module}: {}", scenario.mode);
        let heavy;
        let inputs = if scenario.heavy {
            match Inputs::heavy(test) {
                Some(inputs) => {
                    heavy = inputs;
                    &heavy
                }
                None => continue,
            }
        } else {
            &light
        };
        failures.extend(each(&name, scenario.run, inputs));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Run `command` once under `cancel`, in fresh output and scratch
/// directories, and check what is left: on `Cancelled`, nothing in either.
/// Returns whether the run was cancelled (it can complete when the token is
/// set to trip at a checkpoint the run never reaches).
fn run_once(
    name: &str,
    command: Command,
    inputs: &Inputs,
    cancel: pbfhogg::host::Invocation,
) -> Result<bool, String> {
    let out = crate::common::test_scratch();
    let scratch = crate::common::test_scratch();
    #[cfg(pbfhogg_test_hooks)]
    let token = cancel.clone();
    let host = host_with(cancel, inputs);
    let result = command(inputs, out.path(), scratch.path(), &host);
    drop(host);
    // Every command, whatever its outcome, leaves its invocation with no
    // call guard, no permit and nothing in its census once it has returned
    // and everything it handed out is gone.
    #[cfg(pbfhogg_test_hooks)]
    {
        let rest = token.capabilities();
        if rest != pbfhogg::host::Capabilities::default() {
            return Err(format!("{name}: capabilities left at rest: {rest:?}"));
        }
    }
    match result {
        Ok(()) => Ok(false),
        Err(e) if e.is_cancelled() => {
            if !entries(out.path()).is_empty() {
                return Err(format!(
                    "{name}: a cancelled run left {:?} in the output directory",
                    entries(out.path())
                ));
            }
            if !entries(scratch.path()).is_empty() {
                return Err(format!(
                    "{name}: a cancelled run left {:?} in the scratch parent",
                    entries(scratch.path())
                ));
            }
            Ok(true)
        }
        Err(e) => Err(format!(
            "{name}: expected Cancelled, got {:?}: {}",
            e.kind(),
            e.report()
        )),
    }
}

/// A token tripped before the call stops every scenario of `module`.
fn sweep_before_start(test: &str, module: &str) {
    sweep(test, module, |name, command, inputs| {
        let cancel = pbfhogg::host::Invocation::new();
        cancel.cancel();
        match run_once(name, command, inputs, cancel) {
            Ok(true) => Vec::new(),
            Ok(false) => vec![format!("{name}: completed under a tripped token")],
            Err(e) => vec![e],
        }
    });
}

/// The result of targeting one ordinal of a fixture's counting run.
/// A missing callback is a coverage failure, not a successful cancellation.
#[cfg(pbfhogg_test_hooks)]
#[derive(Debug)]
enum CheckpointRun {
    Unreached { observed: u64 },
    Cancelled,
    CompletedAfterDecision { authorized: u64 },
}

#[cfg(pbfhogg_test_hooks)]
fn run_at_checkpoint(
    name: &str,
    command: Command,
    inputs: &Inputs,
    k: u64,
) -> Result<CheckpointRun, String> {
    let token = pbfhogg::host::Invocation::new();
    let outcome = std::sync::Arc::new(std::sync::Mutex::new(None));
    let seen = std::sync::Arc::clone(&outcome);
    token.on_check(move |ordinal, token| {
        if ordinal == k {
            *seen.lock().unwrap() = Some(token.cancel());
        }
    });
    let cancelled = run_once(name, command, inputs, token.clone())?;
    let fired = *outcome.lock().unwrap();
    match (fired, cancelled) {
        (None, _) => Ok(CheckpointRun::Unreached {
            observed: token.checks_passed(),
        }),
        (Some(_), true) => Ok(CheckpointRun::Cancelled),
        (Some(pbfhogg::CancelOutcome::AfterDecision { authorized }), false) => {
            // Success is allowed only with evidence that publication had
            // already been authorized when this callback tripped the token.
            Ok(CheckpointRun::CompletedAfterDecision { authorized })
        }
        (Some(outcome), false) => Err(format!(
            "{name}: checkpoint {k} fired but the run completed after {outcome:?}"
        )),
    }
}

/// A strict per-scenario replay budget, independent of checkpoint count.
#[cfg(pbfhogg_test_hooks)]
fn checkpoint_sample(count: u64) -> Vec<u64> {
    if count <= 64 {
        (1..=count).collect()
    } else {
        (0..64_u64)
            .map(|n| 1 + (count - 1) / 63 * n + (count - 1) % 63 * n / 63)
            .collect()
    }
}

#[cfg(pbfhogg_test_hooks)]
#[test]
fn checkpoint_sampling_has_a_strict_budget_and_includes_endpoints() {
    for count in [0, 1, 63, 64, 65, 127, 128, 1000, u64::MAX] {
        let sample = checkpoint_sample(count);
        assert_eq!(
            sample.len(),
            usize::try_from(count.min(64)).expect("bounded length")
        );
        assert!(sample.windows(2).all(|pair| pair[0] < pair[1]));
        if count > 0 {
            assert_eq!(sample.first(), Some(&1));
            assert_eq!(sample.last(), Some(&count));
        }
    }
}

/// Sample fixture checkpoint ordinals for every scenario of `module`. Each
/// reached target must cancel or report an already decided publication.
#[cfg(pbfhogg_test_hooks)]
fn sweep_sampled_ordinals(test: &str, module: &str) {
    sweep(test, module, |name, command, inputs| {
        let mut failures = Vec::new();
        let counter = pbfhogg::host::Invocation::new();
        counter.on_check(|_, _| {});
        match run_once(name, command, inputs, counter.clone()) {
            Ok(false) => {}
            Ok(true) => return vec![format!("{name}: counting run unexpectedly cancelled")],
            Err(e) => return vec![e],
        }
        let count = counter.checks_passed();
        if count == 0 {
            return vec![format!("{name}: passed no checkpoint")];
        }
        let mut cancelled = 0_u64;
        // Exhaust at most 64 ordinals; otherwise sample exactly 64 evenly
        // spaced targets including first and last. Callback ordinals are not
        // source-site coverage and parallel scheduling can change reachability.
        for k in checkpoint_sample(count) {
            match run_at_checkpoint(name, command, inputs, k) {
                Ok(CheckpointRun::Cancelled) => cancelled += 1,
                Ok(CheckpointRun::CompletedAfterDecision { authorized }) => {
                    assert!(authorized > 0, "publication ordinals start at one");
                }
                Ok(CheckpointRun::Unreached { observed }) => failures.push(format!(
                    "{name}: target checkpoint {k} of {count} was not reached; observed {observed}"
                )),
                Err(e) => failures.push(format!("{e} (target checkpoint {k} of {count})")),
            }
        }
        if cancelled == 0 {
            failures.push(format!("{name}: no tripped checkpoint cancelled the run"));
        }
        failures
    });
}

/// At observed checkpoints of every scenario of `module`, the census of
/// issued outputs, writers, readers and buffers is empty. This cannot rule
/// out a value issued and retired entirely between two observations.
#[cfg(pbfhogg_test_hooks)]
fn sweep_census(test: &str, module: &str) {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    sweep(test, module, |name, command, inputs| {
        let mut failures = Vec::new();
        let token = pbfhogg::host::Invocation::new();
        let issued = std::sync::Arc::new(AtomicU64::new(0));
        let seen = std::sync::Arc::clone(&issued);
        let observations = std::sync::Arc::new(AtomicU64::new(0));
        let observed = std::sync::Arc::clone(&observations);
        token.on_check(move |_, token| {
            observed.fetch_add(1, Relaxed);
            let c = token.capabilities();
            seen.fetch_max(c.session + c.writer + c.output + c.buffer, Relaxed);
        });
        match run_once(name, command, inputs, token) {
            Ok(false) => {}
            Ok(true) => failures.push(format!("{name}: census run unexpectedly cancelled")),
            Err(e) => failures.push(e),
        }
        if observations.load(Relaxed) == 0 {
            failures.push(format!("{name}: census callback observed no checkpoints"));
        }
        if issued.load(Relaxed) != 0 {
            failures.push(format!("{name}: issued a value of its own to the census"));
        }
        failures
    });
}

/// A public command reached from inside another (`sort` runs `sort_with`)
/// is a nested entry: it mints its own call guard while the outer one is
/// held, so a checkpoint inside it sees two, and both retire.
#[cfg(pbfhogg_test_hooks)]
#[test]
fn a_nested_entry_mints_its_own_guard() {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    let inputs = Inputs::new();
    let token = pbfhogg::host::Invocation::new();
    let at_first = std::sync::Arc::new(AtomicU64::new(0));
    let seen = std::sync::Arc::clone(&at_first);
    token.on_check(move |ordinal, token| {
        // The first checkpoint is sort_with's, before any writer (whose
        // methods are entries too) exists.
        if ordinal == 1 {
            seen.store(token.capabilities().guards, Relaxed);
        }
    });
    assert_eq!(run_once("sort", sort, &inputs, token.clone()), Ok(false));
    assert_eq!(at_first.load(Relaxed), 2, "sort and its nested sort_with");
    assert_eq!(token.capabilities(), pbfhogg::host::Capabilities::default());
}

/// Publication checks the token once, as its decision. A token tripped after
/// that (here, by a `before_publish` hook, which runs after the decision)
/// does not interrupt it.
#[test]
fn a_decided_publication_completes() {
    let dir = crate::common::test_scratch();
    let dest = dir.path().join("out.osm.pbf");
    let host = crate::common::test_host();
    let output = crate::common::out(&dest).bind(&host.invocation).unwrap();
    let mut writer = output
        .pbf_writer(
            Default::default(),
            &pbfhogg::block_builder::HeaderBuilder::new()
                .build()
                .unwrap(),
            pbfhogg::write::OutputBackend::Buffered,
            &host,
        )
        .unwrap();
    writer.flush().unwrap();
    drop(writer);
    let cancel = host.invocation.clone();
    let tripped = host.invocation.clone();
    let outcome = std::sync::Arc::new(std::sync::Mutex::new(None));
    let seen = std::sync::Arc::clone(&outcome);
    output
        .before_publish(move || *seen.lock().unwrap() = Some(tripped.cancel()))
        .publish()
        .expect("a decided publication runs to completion");
    assert!(cancel.is_cancelled());
    assert!(dest.is_file());
    assert_eq!(
        *outcome.lock().unwrap(),
        Some(pbfhogg::CancelOutcome::AfterDecision { authorized: 1 })
    );
}

/// The decision refuses an output whose token was tripped before
/// publication: `Cancelled`, nothing installed, the staging entry gone.
#[test]
fn publication_refuses_once_the_token_is_tripped() {
    let dir = crate::common::test_scratch();
    let dest = dir.path().join("out.osc");
    let cancel = pbfhogg::host::Invocation::new();
    let output = crate::common::out(&dest).bind(&cancel).unwrap();
    let mut writer = output.writer().unwrap();
    std::io::Write::write_all(&mut writer, b"data").unwrap();
    writer.finish().unwrap();
    assert_eq!(
        cancel.cancel(),
        pbfhogg::CancelOutcome::BeforeDecision { refused: 1 }
    );
    let err = output.publish().expect_err("refused");
    assert!(err.is_cancelled(), "{err}");
    assert!(entries(dir.path()).is_empty(), "{:?}", entries(dir.path()));
}

/// A write through a cancelled `OutputWriter` fails the session, so even a
/// caller that ignores the error cannot publish what it wrote.
#[test]
fn a_cancelled_output_writer_cannot_be_published() {
    let dir = crate::common::test_scratch();
    let cancel = pbfhogg::host::Invocation::new();
    let output = crate::common::out(dir.path().join("out.txt"))
        .bind(&cancel)
        .unwrap();
    let writer = output.writer().unwrap();
    // The writer is no owner: tripping the token decides the output.
    cancel.cancel();
    let err = writer.finish().expect_err("finish observes the token");
    assert!(err.is_cancelled(), "{err}");
    let err = output
        .publish()
        .expect_err("a cancelled output is not published");
    assert!(err.is_cancelled(), "{err}");
    assert!(entries(dir.path()).is_empty(), "{:?}", entries(dir.path()));
}

/// After the decision, publication's own outcome stands: an unsynced
/// directory is reported as such, not turned into a cancellation.
#[cfg(pbfhogg_test_hooks)]
#[test]
fn a_publication_failure_after_the_decision_is_not_a_cancellation() {
    let dir = crate::common::test_scratch();
    let dest = dir.path().join("out.osc");
    let cancel = pbfhogg::host::Invocation::new();
    let output = crate::common::out(&dest).bind(&cancel).unwrap();
    cancel
        .faults()
        .arm_publish_fault(pbfhogg::output::PublishFault::DirSync);
    let mut writer = output.writer().unwrap();
    std::io::Write::write_all(&mut writer, b"data").unwrap();
    writer.finish().unwrap();
    let tripped = cancel.clone();
    let err = output
        .before_publish(move || {
            tripped.cancel();
        })
        .publish()
        .expect_err("the injected directory sync failure");
    assert!(!err.is_cancelled(), "{err}");
    assert!(
        matches!(
            err.kind(),
            pbfhogg::ErrorKind::Output(pbfhogg::OutputError::Unsynced { .. })
        ),
        "{err}"
    );
    assert!(dest.is_file(), "the output was published");
}

/// Each invocation observes its own token: two sorts running at once, one
/// cancelled mid-run, the other completing.
#[cfg(pbfhogg_test_hooks)]
#[test]
fn a_token_stops_only_the_runs_that_observe_it() {
    let inputs = Inputs::new();
    let stopped = pbfhogg::host::Invocation::new();
    stopped.on_check(|ordinal, token| {
        if ordinal == 3 {
            token.cancel();
        }
    });
    // No HangBound: each scoped run cancels itself from its own check and
    // waits on nothing the test or the other run supplies, so the scope is
    // two operations as unbounded as one on the test thread.
    let (cancelled, completed) = std::thread::scope(|scope| {
        let a = scope.spawn(|| run_once("sort a", sort, &inputs, stopped.clone()));
        let b = scope.spawn(|| run_once("sort b", sort, &inputs, pbfhogg::host::Invocation::new()));
        (a.join().unwrap(), b.join().unwrap())
    });
    assert_eq!(cancelled, Ok(true));
    assert_eq!(completed, Ok(false));
}
